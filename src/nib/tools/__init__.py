"""Core agent tools for nib.

All tools are registered here and executed exclusively through ToolExecutor
to enforce the full permission model (see docs/tech/permissions.md).
"""

# Import core_tools to trigger registrations
from . import core_tools  # noqa: F401
from .executor import ToolExecutor, default_executor
from .models import (
    ApprovalMode,
    PermissionLevel,
    ToolCall,
    ToolExecutionRecord,
    ToolMetadata,
    ToolResult,
)
from .registry import get_tool_metadata, list_tools, register_tool

__all__ = [
    "ApprovalMode",
    "PermissionLevel",
    "ToolCall",
    "ToolExecutionRecord",
    "ToolExecutor",
    "ToolMetadata",
    "ToolResult",
    "default_executor",
    "get_tool_metadata",
    "list_tools",
    "register_tool",
]
