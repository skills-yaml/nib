"""Session persistence layer.

All conversation and tool-calling history is stored as plain files under
<project>/.nib/sessions/.

This replaces the previous global SQLite-based workload model that used
projects and tasks.
"""

from __future__ import annotations

import json
import uuid
from pathlib import Path

from nib.core.models import Session, SessionMessage, ToolCallRecord


class SessionStore:
    """File-based persistence for sessions inside the project's .nib folder."""

    def __init__(self, project_root: Path | None = None) -> None:
        if project_root is None:
            project_root = Path.cwd()

        self.project_root = project_root.resolve()
        self.nib_dir = self.project_root / ".nib"
        self.sessions_dir = self.nib_dir / "sessions"

        self.nib_dir.mkdir(parents=True, exist_ok=True)
        self.sessions_dir.mkdir(parents=True, exist_ok=True)

    def _path(self, session_id: str) -> Path:
        return self.sessions_dir / f"{session_id}.json"

    def create_session(self) -> Session:
        session = Session(id=str(uuid.uuid4()))
        self.save_session(session)
        return session

    def load_session(self, session_id: str) -> Session | None:
        path = self._path(session_id)
        if not path.exists():
            return None
        data = json.loads(path.read_text(encoding="utf-8"))
        return Session.model_validate(data)

    def save_session(self, session: Session) -> None:
        path = self._path(session.id)
        path.write_text(session.model_dump_json(indent=2), encoding="utf-8")

    def list_sessions(self) -> list[str]:
        return sorted(p.stem for p in self.sessions_dir.glob("*.json"))

    def append_message(self, session_id: str, role: str, content: str) -> Session:
        session = self.load_session(session_id) or Session(id=session_id)
        session.messages.append(SessionMessage(role=role, content=content))
        self.save_session(session)
        return session

    def record_tool_call(self, record: ToolCallRecord | dict) -> None:
        """Accept either ToolCallRecord (from core or tools) or dict."""
        if isinstance(record, dict):
            record = ToolCallRecord(**record)
        elif hasattr(record, "model_dump"):
            # Pydantic v2 compat (could be from tools.models)
            data = record.model_dump()
            record = ToolCallRecord(**data)
        session = self.load_session(record.session_id) or Session(id=record.session_id)
        session.tool_calls.append(record)
        self.save_session(session)

    def get_latest_session(self) -> Session | None:
        ids = self.list_sessions()
        if not ids:
            return None
        return self.load_session(ids[-1])
