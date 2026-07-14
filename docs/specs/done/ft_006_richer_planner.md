# FT-006: Richer Planner (Symphony-Style)

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Enhance the existing `AgentLoop` to support multi-step reasoning and a dedicated planning phase before execution, inspired by Symphony-style workflows.

## Problem Statement
Currently, the LLM takes a greedy approach, executing tool calls immediately based on the current context. For complex tasks, this can lead to getting stuck in local maxima or breaking the codebase without a holistic strategy.

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
- `cargo fmt && task check` pass without issues.
- All related unit tests pass.

## Affected Areas
- `src/agent/planner.rs` (new file)
- `src/agent/loop.rs` (AgentLoop logic)
- `src/session.rs` (Session schema for plan storage)
- `src/agent/state.rs` (State machine updates)
- `src/llm/types.rs` (if new parsing types needed for planner)

## Validation Gates
- `task check`
- `task test`
- Manual verification of a multi-step task run resulting in a saved plan.
