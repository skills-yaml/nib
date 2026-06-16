"""Domain models for nib (workload, projects, tasks)."""

from __future__ import annotations

from datetime import UTC, datetime
from enum import StrEnum

from pydantic import BaseModel, Field


class TaskStatus(StrEnum):
    TODO = "todo"
    IN_PROGRESS = "in_progress"
    BLOCKED = "blocked"
    DONE = "done"
    CANCELLED = "cancelled"


class Priority(StrEnum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"


class Task(BaseModel):
    """A unit of work in the workload model."""

    id: str
    title: str
    description: str = ""
    status: TaskStatus = TaskStatus.TODO
    priority: Priority = Priority.MEDIUM
    project_id: str | None = None
    parent_id: str | None = None  # for subtasks / hierarchy
    estimate_minutes: int | None = None
    due: datetime | None = None
    tags: list[str] = Field(default_factory=list)
    links: list[str] = Field(default_factory=list)  # GitHub issues, PRs, Notion pages, etc.
    created_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    updated_at: datetime = Field(default_factory=lambda: datetime.now(UTC))

    model_config = {"extra": "forbid"}


class Project(BaseModel):
    """A container for related work."""

    id: str
    name: str
    description: str = ""
    root_path: str | None = None  # local filesystem root for the project
    created_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    updated_at: datetime = Field(default_factory=lambda: datetime.now(UTC))


class WorkloadSnapshot(BaseModel):
    """Point-in-time view of the entire workload."""

    projects: list[Project]
    tasks: list[Task]
    generated_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
