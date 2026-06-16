"""Workload persistence and query layer (SQLite-backed).

Extended with tool execution + approval audit records as required by the
permissions deep-dive and FT-001 spec.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import TYPE_CHECKING

import aiosqlite

from nib.core.models import WorkloadSnapshot

if TYPE_CHECKING:
    from nib.tools.models import ToolExecutionRecord


class WorkloadStore:
    """Local-first workload database."""

    def __init__(self, db_path: Path | None = None) -> None:
        self.db_path = db_path or (Path.home() / ".nib" / "workload.db")
        self.db_path.parent.mkdir(parents=True, exist_ok=True)

    async def init_db(self) -> None:
        """Create tables if they don't exist (including tool audit tables)."""
        async with aiosqlite.connect(self.db_path) as db:
            await db.execute(
                """
                CREATE TABLE IF NOT EXISTS projects (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    description TEXT,
                    root_path TEXT,
                    created_at TEXT,
                    updated_at TEXT
                )
                """
            )
            await db.execute(
                """
                CREATE TABLE IF NOT EXISTS tasks (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    description TEXT,
                    status TEXT,
                    priority TEXT,
                    project_id TEXT,
                    parent_id TEXT,
                    estimate_minutes INTEGER,
                    due TEXT,
                    tags TEXT,
                    links TEXT,
                    created_at TEXT,
                    updated_at TEXT
                )
                """
            )
            await db.execute(
                """
                CREATE TABLE IF NOT EXISTS tool_executions (
                    id TEXT PRIMARY KEY,
                    task_id TEXT NOT NULL,
                    tool_name TEXT NOT NULL,
                    arguments TEXT,
                    success BOOLEAN,
                    output TEXT,
                    error TEXT,
                    duration_seconds REAL,
                    approval_granted BOOLEAN,
                    approval_source TEXT,
                    project_root TEXT,
                    worktree_path TEXT,
                    executed_at TEXT,
                    FOREIGN KEY(task_id) REFERENCES tasks(id)
                )
                """
            )
            await db.commit()

    async def get_snapshot(self) -> WorkloadSnapshot:
        """Return a full in-memory snapshot."""
        # TODO: real queries
        return WorkloadSnapshot(projects=[], tasks=[])

    async def record_tool_execution(self, record: ToolExecutionRecord) -> None:
        """Persist a tool call + approval decision (core of permissions audit)."""
        async with aiosqlite.connect(self.db_path) as db:
            await db.execute(
                """
                INSERT OR REPLACE INTO tool_executions
                (id, task_id, tool_name, arguments, success, output, error,
                 duration_seconds, approval_granted, approval_source,
                 project_root, worktree_path, executed_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    record.id,
                    record.task_id,
                    record.tool_name,
                    json.dumps(record.arguments),
                    record.result.success,
                    json.dumps(record.result.output) if record.result.output else None,
                    record.result.error,
                    record.result.duration_seconds,
                    record.result.approval_granted,
                    record.result.approval_source,
                    record.project_root,
                    record.worktree_path,
                    record.executed_at.isoformat(),
                ),
            )
            await db.commit()

    async def get_tool_history_for_task(self, task_id: str) -> list[dict]:
        """Retrieve audit trail for a task (used by reconciler and UI)."""
        async with aiosqlite.connect(self.db_path) as db:
            db.row_factory = aiosqlite.Row
            async with db.execute(
                "SELECT * FROM tool_executions WHERE task_id = ? ORDER BY executed_at",
                (task_id,),
            ) as cursor:
                rows = await cursor.fetchall()
                return [dict(row) for row in rows]
