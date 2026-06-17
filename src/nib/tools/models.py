"""Pydantic models for tools, permissions, calls, and results.

This is the foundation for the ToolExecutor and permission system.
"""

from __future__ import annotations

from datetime import UTC, datetime
from enum import StrEnum
from typing import Any, Literal

from pydantic import BaseModel, Field


class PermissionLevel(StrEnum):
    """Classification of tool actions for permission decisions."""

    READ_ONLY = "read_only"
    SAFE = "safe"
    DESTRUCTIVE = "destructive"
    NETWORK = "network"


class ApprovalMode(StrEnum):
    """Approval modes for the agent (inspired by advanced agent runtimes + custom for nib)."""

    MANUAL = "manual"  # Always prompt for non-read-only
    SMART = "smart"  # LLM/rules assisted auto-approve for safe actions
    POLICY = "policy"  # Only explicit prior grants or policy rules
    OFF = "off"  # YOLO - use with extreme caution


class ToolMetadata(BaseModel):
    """Static metadata for a registered tool."""

    name: str
    description: str
    permission_level: PermissionLevel
    requires_approval: bool = True
    requires_worktree: bool = False
    mcp_exposable: bool = True
    dangerous_patterns: list[str] = Field(default_factory=list)  # for run_terminal


class ToolCall(BaseModel):
    """A request to execute a tool."""

    tool_name: str
    arguments: dict[str, Any]
    task_id: str | None = None
    project_root: str | None = None
    worktree_path: str | None = None
    requested_at: datetime = Field(default_factory=lambda: datetime.now(UTC))


class ToolResult(BaseModel):
    """Result of a tool execution."""

    tool_name: str
    success: bool
    output: Any | None = None
    error: str | None = None
    duration_seconds: float
    approval_granted: bool = False
    approval_source: Literal["user", "policy", "smart", "yolo"] | None = None
    redacted: bool = False


class ApprovalRequest(BaseModel):
    """Request for user (or policy) approval."""

    tool_name: str
    arguments: dict[str, Any]
    permission_level: PermissionLevel
    reason: str  # From planner or agent context
    risk_explanation: str
    suggested_command_or_patch: str | None = None
    task_id: str | None = None


class ApprovalDecision(BaseModel):
    """Result of an approval request."""

    granted: bool
    source: Literal["user", "policy", "smart", "denied"]
    approver_note: str | None = None
    granted_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    expires_for_task: bool = True  # Per-task grant by default


class ToolExecutionRecord(BaseModel):
    """Audit record stored in workload DB."""

    id: str
    task_id: str
    tool_name: str
    arguments: dict[str, Any]
    result: ToolResult
    approval: ApprovalDecision | None = None
    project_root: str | None = None
    worktree_path: str | None = None
    executed_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
