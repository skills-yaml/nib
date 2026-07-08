# T008: End-to-End Tests and Sequence Diagram Validation

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

## Problem Statement

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

## Proposed Design

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