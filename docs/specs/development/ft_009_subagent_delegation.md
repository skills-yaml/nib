# FT-009: Deep Sub-Agent Delegation

**Status:** Development
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

## Scope
- Create a `tools::delegation` module with `spawn_subagent` and `merge_subagent_worktree` tools.
- Implement `spawn_subagent` to initialize a new session and spawn a child Tokio task running `run_agent_loop` recursively.
- The `spawn_subagent` should return a job/session ID to the parent agent.
- Modify `ToolRegistry` to include the delegation tools.

## Acceptance Criteria
- `spawn_subagent` correctly creates a child session and executes a subagent run asynchronously in the background.
- `merge_subagent_worktree` correctly pulls/merges changes from a subagent's worktree.
- `cargo fmt && task check` pass.
- Unit or integration tests demonstrate subagent spawning and completion.

## Affected Areas
- `src/tools/delegation.rs` (new module)
- `src/tools/registry.rs` (registering new tools)
- `src/tools/core.rs` (dispatching delegation tools)
- `src/tools/mod.rs` (exporting delegation)

## Validation Gates
- `task check`
- `task test`
