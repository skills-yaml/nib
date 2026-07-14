# FT-010: MCP Server Exposing the Agent Loop

**Status:** Backlog
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Build an MCP server embedded in `nib` that exposes the `nib` agent loop itself as a tool, allowing other tools or IDES (like Claude Desktop) to delegate workloads to the `nib` CLI safely.

## Problem Statement
`nib` currently consumes MCP tools (as a client), but it cannot be easily orchestrated by *other* AI systems that rely on MCP.

## Goals
- Implement an MCP server endpoint in `src/integrations/mcp_server.rs`.
- Expose tools like `nib_run(goal)` and `nib_get_status(session_id)`.
- Allow external systems to leverage `nib`'s gated execution model, hybrid sandboxing, and session persistence natively.
