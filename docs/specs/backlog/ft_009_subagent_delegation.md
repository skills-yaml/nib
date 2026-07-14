# FT-009: Deep Sub-Agent Delegation

**Status:** Backlog
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Allow the primary `nib` agent to spawn, delegate to, and reconcile output from ephemeral sub-agents operating in parallel.

## Problem Statement
A single agent loop struggles with massive codebase refactors. It loses context quickly and cannot parallelize independent tasks. 

## Goals
- Provide a `spawn_subagent(goal, worktree)` tool.
- Sub-agents operate in their own linked session and isolated git worktree.
- The primary agent acts as the Orchestrator, dispatching tasks and aggregating results.
- Implement reconciliation tools to merge the sub-agent's worktree back into the main branch once tests pass.
