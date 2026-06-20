"""Worktree manager for safe, isolated execution.

Implements the isolation layer from the permissions deep-dive.
Used by ToolExecutor for apply_patch and destructive run_terminal when possible.
"""

from __future__ import annotations

import subprocess
import uuid
from pathlib import Path


class WorktreeManager:
    """Manages git worktrees for task-isolated changes."""

    def __init__(self, repo_root: Path):
        self.repo_root = repo_root.resolve()
        self.worktrees: dict[str, Path] = {}  # session_id -> worktree_path

    def create_for_task(self, session_id: str, branch: str | None = None) -> Path:
        """Create an isolated worktree for the given session.

        Returns the path to the worktree.
        """
        if session_id in self.worktrees:
            return self.worktrees[session_id]

        wt_name = f"nib-session-{session_id}-{uuid.uuid4().hex[:8]}"
        wt_path = self.repo_root / ".worktrees" / wt_name
        wt_path.parent.mkdir(parents=True, exist_ok=True)

        base = branch or "HEAD"

        try:
            # Create worktree
            subprocess.check_call(
                ["git", "worktree", "add", "-b", f"nib/{session_id}/{wt_name}", str(wt_path), base],
                cwd=self.repo_root,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self.worktrees[session_id] = wt_path
            return wt_path
        except subprocess.CalledProcessError as e:
            raise RuntimeError(f"Failed to create worktree for session {session_id}: {e}") from e

    def get_path(self, session_id: str) -> Path | None:
        return self.worktrees.get(session_id)

    def cleanup(self, session_id: str, remove_branch: bool = False) -> None:
        """Remove the worktree for the session.

        Optionally delete the branch.
        """
        wt_path = self.worktrees.pop(session_id, None)
        if not wt_path or not wt_path.exists():
            return

        try:
            subprocess.check_call(
                ["git", "worktree", "remove", "--force", str(wt_path)],
                cwd=self.repo_root,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            if remove_branch:
                # branch name is in the path or we can infer
                pass  # TODO: track branch name if needed
        except subprocess.CalledProcessError:
            # Best effort cleanup
            pass

    def status(self, session_id: str) -> str:
        """Return short git status inside the worktree."""
        wt_path = self.get_path(session_id)
        if not wt_path:
            return "no worktree"
        try:
            out = subprocess.check_output(
                ["git", "status", "--porcelain", "--branch"],
                cwd=wt_path,
                text=True,
            )
            return out.strip()
        except Exception:
            return "error fetching status"
