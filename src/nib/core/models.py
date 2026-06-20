"""Domain models for nib.

The persistence model has changed:
- No more global "projects" and "tasks".
- Sessions (conversations + tool calls) are stored as files in the project's .nib/sessions/ directory.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pydantic import BaseModel, Field


class SessionMessage(BaseModel):
    """One turn in a conversation."""
    role: str  # "user", "assistant", "system", "tool"
    content: str
    timestamp: datetime = Field(default_factory=lambda: datetime.now(UTC))


class ToolCallRecord(BaseModel):
    """A recorded tool invocation inside a session."""
    id: str
    session_id: str
    tool_name: str
    arguments: dict[str, object]
    result: dict[str, object] | None = None
    error: str | None = None
    duration_seconds: float | None = None
    worktree_path: str | None = None
    timestamp: datetime = Field(default_factory=lambda: datetime.now(UTC))


class Session(BaseModel):
    """A conversation session with history."""
    id: str
    started_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    messages: list[SessionMessage] = Field(default_factory=list)
    tool_calls: list[ToolCallRecord] = Field(default_factory=list)
