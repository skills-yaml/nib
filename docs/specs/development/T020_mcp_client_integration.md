# T020: MCP Client Integration

**Status:** Development
**Related:** [FT-005](../development/ft_005_pure_rust_core_migration.md), [Ecosystem Integration](../../tech/ecosystem_integration.md)

## Scope

nib needs to connect to MCP (Model Context Protocol) servers to expose their tools to the internal agent loop.

## Acceptance Criteria

- [ ] Create `src/integrations/mcp.rs`.
- [ ] Implement a basic MCP client manager that reads server configuration (e.g. from `.nib/config.toml` or `~/.grok/mcp.json`).
- [ ] Expose discovered MCP tools through the `ToolExecutor` / `registry`.
- [ ] Ensure MCP tool execution routes through the MCP client but still triggers `ToolExecutor` approval/recording logic.

## Affected Areas

- `src/integrations/mcp.rs`
- `src/config/mod.rs` (if adding MCP server config)
- `src/tools/executor.rs` and `src/tools/registry.rs` (dynamic tools vs static tools)

## Validation Gates

- Must pass `task check` and `task test`.
- Demonstrate one mock MCP tool being called.
