# FT-010: MCP Server Exposing the Agent Loop

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Build an MCP server embedded in `nib` that exposes the `nib` agent loop itself as a tool, allowing other tools or IDES (like Claude Desktop) to delegate workloads to the `nib` CLI safely.

## Problem Statement
`nib` currently consumes MCP tools (as a client), but it cannot be easily orchestrated by *other* AI systems that rely on MCP.

## Goals
- Implement an MCP server endpoint in `src/integrations/mcp_server.rs`.
- Expose tools like `nib_run(goal)` and `nib_get_status(session_id)`.
- Allow external systems to leverage `nib`'s gated execution model, hybrid sandboxing, and session persistence natively.

## Scope
- Create `src/integrations/mcp_server.rs` module.
- Implement an MCP server that responds over stdio.
- Add tool `nib_run` to start a background `nib` task.
- Add tool `nib_get_status` to query the status of an agent run using its session_id.
- Add a new CLI command to start the MCP server (e.g. `nib mcp-server`).

## Acceptance Criteria
- `nib mcp-server` starts an MCP stdio server.
- The server advertises `nib_run` and `nib_get_status` tools.
- `nib_run` starts a task via `spawn_subagent` logic or directly spawning `run_agent_loop`.
- `nib_get_status` retrieves the status of the background task.
- Code passes `cargo fmt` and `task check`.
- Tests verify the JSON-RPC interface for the server.

## Affected Areas
- `src/integrations/mcp_server.rs` (new)
- `src/integrations/mod.rs`
- `src/main.rs` (new CLI command)

## Validation Gates
- `task check`
- `task test`
