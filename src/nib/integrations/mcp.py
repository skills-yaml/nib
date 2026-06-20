"""MCP (Model Context Protocol) integration for nib.

nib acts as both:
- MCP client: consumes tools from GitHub, Notion, Linear, etc.
- MCP server: exposes nib's sessions, planning, and execution capabilities.

This allows seamless interoperability with the Grok TUI, Claude, and similar agent environments.
"""

from __future__ import annotations

from typing import Any

# Placeholder — real implementation will use the `mcp` package
# pip/uv install mcp


class MCPClientManager:
    """Manages connections to external MCP servers."""

    def __init__(self) -> None:
        self.connected_servers: dict[str, Any] = {}

    def connect(self, name: str, **config) -> None:
        """Connect to an MCP server (stdio, sse, or http)."""
        # TODO: implement using mcp.ClientSession or equivalent
        print(f"[MCP] Would connect to {name} with config {config}")
        self.connected_servers[name] = {"status": "stub"}

    def list_tools(self, server: str | None = None) -> list[dict]:
        """Return available tools from connected MCP servers."""
        # In real impl: call list_tools on each session
        return []

    def call_tool(self, server: str, tool: str, arguments: dict) -> Any:
        """Call a tool on a remote MCP server (must go through permissions)."""
        print(f"[MCP] Would call {server}.{tool} with {arguments}")
        return {"result": "stub", "server": server}


class MCPServer:
    """Serves nib's tools (from ToolRegistry/Executor) and other capabilities via MCP.

    Other agents (Claude, Grok, and similar) can call nib for safe execution.
    Permissions are still enforced inside the executor.
    """

    def __init__(self, executor=None) -> None:
        self.executor = executor  # ToolExecutor instance

    def get_tools(self) -> list[dict]:
        """Expose core tools (via registry) + workload/planning."""
        from nib.tools.registry import list_tools

        tools = []
        for meta in list_tools():
            tools.append(
                {
                    "name": f"nib_{meta.name}",
                    "description": meta.description,
                    "permission_level": meta.permission_level.value,
                }
            )
        tools.extend(
            [
                {
                    "name": "nib_list_sessions",
                    "description": "List sessions (conversations + tool calls) stored in .nib/sessions/",
                },
                {
                    "name": "nib_get_context",
                    "description": "Get loaded AGENTS.md + skills for current session",
                },
            ]
        )
        return tools

    async def handle_tool_call(
        self, name: str, arguments: dict, session_id: str | None = None
    ) -> dict:
        """Dispatch to executor (permissions applied) or stubs."""
        if name.startswith("nib_"):
            tool_name = name[4:]
            from nib.tools.registry import list_tools

            if tool_name in [m.name for m in list_tools()]:
                if self.executor:
                    from nib.tools.models import ToolCall

                    call = ToolCall(tool_name=tool_name, arguments=arguments, session_id=session_id)
                    result = await self.executor.execute(call, session_id=session_id)
                    return result.model_dump()
        return {"status": "not_implemented", "tool": name}
