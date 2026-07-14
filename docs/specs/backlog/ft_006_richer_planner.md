# FT-006: Richer Planner (Symphony-Style)

**Status:** Backlog
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
