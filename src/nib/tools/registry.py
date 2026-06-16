"""Central tool registry with metadata.

Tools are registered here with their PermissionLevel and other metadata.
This allows the executor, MCP layer, and skills to discover and reason about tools.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from nib.tools.models import PermissionLevel, ToolMetadata


_REGISTRY: dict[str, ToolMetadata] = {}


def register_tool(metadata: ToolMetadata) -> None:
    """Register a tool. Call this at import time for each core tool."""
    if metadata.name in _REGISTRY:
        raise ValueError(f"Tool {metadata.name} already registered")
    _REGISTRY[metadata.name] = metadata


def get_tool_metadata(name: str) -> ToolMetadata:
    if name not in _REGISTRY:
        raise KeyError(f"Unknown tool: {name}")
    return _REGISTRY[name]


def list_tools() -> list[ToolMetadata]:
    return list(_REGISTRY.values())


def get_permission_level(name: str) -> PermissionLevel:
    return get_tool_metadata(name).permission_level
