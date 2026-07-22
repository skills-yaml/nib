# T008: End-to-End Tests and Sequence Diagram Validation

**Status:** Done

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

> Historical proposal note: the pytest design and missing-suite statement below
> capture the pre-Rust baseline. The 2026-07-15 reconciliation defines the shipped
> deterministic Rust end-to-end suite.

## Historical Problem Statement (Proposal-Time)

While unit tests exist for components (e.g., tools, context), there is no comprehensive end-to-end test suite exercising the full runtime loop, state machine, compression, permissions, MCP/skills integration, and workload updates. The detailed ASCII Sequence Diagram in T002 (and referenced in other tasks) must be validated against actual implementation to ensure fidelity. Without this, alignment to the target architecture risks drift, and complex interactions (e.g., approval + compression + tool dispatch) may have untested edge cases.

## Goals

- Create e2e tests that simulate full cycles: prompt → context build → LLM/tool decision → approval → execute (in worktree/sandbox) → compress (if triggered) → update memory/workload → response.
- Validate that execution traces match the ASCII Sequence Diagram steps from T002.
- Cover edge cases: compression threshold, approval denial, role invariant violations, cross-session persistence, MCP delegation.
- Integrate with pytest-asyncio and existing test setup.
- Ensure `task test` covers the runtime engine.

## Non-Goals

- Performance benchmarking (focus on correctness first).
- GUI/TUI e2e (CLI-driven tests sufficient).

## Historical Proposed Design

- New tests in `tests/test_runtime_e2e.py`.
- Use mocking for LLM (simulate tool calls/responses) and MCP.
- Assert on state transitions, context size post-compression, workload records, diagram step coverage (e.g., via logging or markers for steps 1-17).
- Fixtures for profiles, sample skills, temp worktrees.
- Parameterize tests for different approval modes and compression scenarios.

## Alternatives Considered

- Rely on manual demo-tool testing: Insufficient for comprehensive coverage and regression prevention.

## Risks and Tradeoffs

- Test flakiness with async/MCP (mitigation: deterministic mocks).

## Rollout Plan

1. Implement core e2e happy path matching diagram.
2. Add edge case tests.
3. Diagram validation script (parse steps and assert in logs).
4. CI integration.

## Validation and Acceptance Criteria

- E2e tests pass for full loop, including optional compression and approvals.
- Diagram steps are explicitly exercised and logged in tests.
- 80%+ coverage on runtime components.
- `task test` includes T008; no regressions in existing tests.

## Open Questions

- How to best assert diagram fidelity without over-coupling tests to ASCII text? (e.g., step markers).

## Reopened Audit (2026-07-15)

Scope: add observable transition traces and deterministic E2E scenarios for mutation,
approval/denial, compression, role errors, persistence, skills, MCP, and reconciliation.

Affected areas: runtime observability, `tests/`, Task coverage gates, and the T002
sequence diagram.

Validation gates: all diagram markers asserted, at least 80% runtime-component
coverage, `task check`, `task test`, and `task coverage`.

## Implementation Reconciliation (2026-07-15)

### Scope

Prove observable Rust runtime sequences for profile/context/skills, planning,
approval, worktree mutation, compression, role invariants, bounds, MCP, delegation,
durable work, and reconciliation.

### Acceptance Criteria

- [x] A deterministic mock-provider E2E asserts the ordered state trace and audited artifacts.
- [x] Approved mutation physically changes only the session worktree; denial has no side effect.
- [x] Compression retains raw history and role violations fail without losing audit state.
- [x] MCP delegation and subagent verification/merge paths are integration tested.
- [x] Detached terminal and scheduled wake behavior is tested across processes.
- [x] Runtime-component line coverage is at least 80 percent (83.00 percent on 2026-07-15).
- [x] Final `task check` and `task test` are green.

### Affected Areas

`tests/test_runtime_e2e.rs`, `tests/delegation.rs`, `tests/durable_tasks.rs`,
`tests/mcp_integration.rs`, runtime observability, and `scripts/check-runtime-coverage.sh`.

### Implementation Evidence

- `src/agent/loop.rs` emits transition traces and audited lifecycle events.
- `scripts/check-runtime-coverage.sh` defines the runtime coverage gate.

### Validation Evidence

- `tests/test_runtime_e2e.rs` contains eight deterministic lifecycle/artifact scenarios.
- `tests/delegation.rs` covers ten persistence, cancellation, policy, verification, and merge scenarios.
- `tests/durable_tasks.rs` covers four detached cross-process scenarios.

### Validation Gates

- [x] Deterministic E2E suites and explicit trace assertions exist.
- [x] `task coverage` at 80 percent or higher (83.00 percent on 2026-07-15).
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

The tests assert the semantic state sequence rather than coupling to every numbered
line of the historical ASCII diagram. This keeps the tests behavior-oriented without
leaving an in-scope validation gap.

## Reopened Audit (2026-07-16)

### Scope

Replace fragmented direct-helper evidence with one deterministic agent-loop scenario
that covers planning, approval, compression, physical mutation, real verification, and
reconciliation in order.

### Acceptance Criteria

- [x] One E2E test asserts the ordered semantic trace for a multi-step coding task.
- [x] The same run records a compression event without deleting raw history.
- [x] The agent applies a physical edit only in its session worktree.
- [x] The agent runs a real compile/test command in that worktree and records its result.
- [x] Plan, messages, tool calls, worktree identity, and reconciliation outcome are
  asserted from the persisted session.

### Affected Areas

`tests/test_runtime_e2e.rs`, Mock LLM fixtures, runtime trace assertions, and coverage.

### Implementation Plan

1. Add a deterministic Mock scenario for a small broken Rust crate.
2. Seed enough prior history to cross the configured compression threshold.
3. Run the normal loop through plan approval, patch, `cargo test`, and reconciliation.
4. Assert the persisted transcript, plan, audit records, worktree, and final event.

### Risks

Nested Cargo execution can become environment-dependent. The fixture has no external
dependencies, uses the existing terminal sandbox path, and validates only observable
worktree and audit behavior.

### Completion Evidence

`tests/test_runtime_e2e.rs::
full_agent_loop_compresses_edits_and_runs_real_cargo_tests_in_one_worktree` drives the
normal planner, approval, compression, patch, terminal, and reconciliation states. It
asserts raw-history preservation, worktree-only mutation, real Rust tests, exact plan
audit IDs, completed plan state, and persisted terminal reconciliation.

### Validation Gates

Focused E2E run, `task check`, `task test`, and `task coverage` at or above 80 percent.
