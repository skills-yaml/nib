# T005: Full Runtime State Machine and Lifecycle

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

## Problem Statement

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

## Proposed Design

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