"""Implementation of the 5 core minimal tools.

These are the functions dispatched by ToolExecutor.
They assume the executor has already handled scoping, approval, worktree, etc.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from nib.tools.models import PermissionLevel, ToolMetadata
from nib.tools.registry import register_tool

# --- Registration (done at import) ---

register_tool(
    ToolMetadata(
        name="read_file",
        description="Read file contents (optionally limited to line range).",
        permission_level=PermissionLevel.READ_ONLY,
        requires_approval=False,
    )
)

register_tool(
    ToolMetadata(
        name="list_directory",
        description="List directory contents.",
        permission_level=PermissionLevel.READ_ONLY,
        requires_approval=False,
    )
)

register_tool(
    ToolMetadata(
        name="grep",
        description="Search files by content (regex or literal).",
        permission_level=PermissionLevel.READ_ONLY,
        requires_approval=False,
    )
)

register_tool(
    ToolMetadata(
        name="apply_patch",
        description="Apply a unified diff/patch. Strongly prefers worktree.",
        permission_level=PermissionLevel.SAFE,
        requires_approval=True,
        requires_worktree=True,
    )
)

register_tool(
    ToolMetadata(
        name="run_terminal",
        description="Execute a shell command. Highest risk tool.",
        permission_level=PermissionLevel.DESTRUCTIVE,  # will be dynamically re-classified
        requires_approval=True,
        requires_worktree=True,
        dangerous_patterns=[
            r"rm\s+-rf",
            r"git\s+reset\s+--hard",
            r"git\s+push\s+--force",
            r"DROP\s+DATABASE",
            r"sudo",
            r"curl\s+.*\|\s*sh",
        ],
    )
)


# --- Actual implementations (called only after executor approval) ---


async def read_file_impl(args: dict[str, Any], cwd: Path | None = None) -> dict:
    path = Path(args["path"])
    if cwd:
        path = (cwd / path).resolve()
    if not path.is_file():
        return {"error": f"File not found: {path}"}
    start = args.get("start_line")
    end = args.get("end_line")
    content = path.read_text(encoding="utf-8", errors="replace")
    lines = content.splitlines(keepends=True)
    if start is not None or end is not None:
        lines = lines[start or 0 : end]
    return {
        "path": str(path),
        "content": "".join(lines),
        "start_line": start,
        "end_line": end,
        "total_lines": len(content.splitlines()),
    }


async def list_directory_impl(args: dict[str, Any], cwd: Path | None = None) -> dict:
    path = Path(args.get("path", "."))
    if cwd:
        path = (cwd / path).resolve()
    recursive = args.get("recursive", False)
    entries = []
    if recursive:
        for p in sorted(path.rglob("*"))[:100]:  # safety limit
            entries.append(
                {"path": str(p.relative_to(path)), "type": "dir" if p.is_dir() else "file"}
            )
    else:
        for p in sorted(path.iterdir()):
            entries.append({"path": p.name, "type": "dir" if p.is_dir() else "file"})
    return {"path": str(path), "entries": entries}


async def grep_impl(args: dict[str, Any], cwd: Path | None = None) -> dict:
    # Very basic implementation. Real version would use ripgrep or similar via terminal.
    pattern = args["pattern"]
    path = Path(args.get("path", "."))
    if cwd:
        path = (cwd / path).resolve()
    max_results = args.get("max_results", 50)
    matches = []
    for file in path.rglob("*") if path.is_dir() else [path]:
        if file.is_file() and len(matches) < max_results:
            try:
                text = file.read_text(errors="ignore")
                for i, line in enumerate(text.splitlines(), 1):
                    if pattern.lower() in line.lower():
                        matches.append({"file": str(file), "line": i, "snippet": line[:200]})
                        if len(matches) >= max_results:
                            break
            except Exception:
                pass
    return {"pattern": pattern, "matches": matches, "truncated": len(matches) >= max_results}


async def apply_patch_impl(args: dict[str, Any], cwd: Path | None = None) -> dict:
    # Placeholder. Real impl will use git apply inside worktree.
    patch = args.get("patch", "")
    target = args.get("path")
    dry = args.get("dry_run", True)
    return {
        "applied": not dry,
        "message": f"[stub] Would apply patch to {target} (dry={dry})",
        "patch_preview": patch[:500],
    }


async def run_terminal_impl(args: dict[str, Any], cwd: Path | None = None) -> dict:
    cmd = args["command"]
    # timeout = args.get("timeout", 30)  # TODO: wire into real subprocess
    # Real version will use asyncio.create_subprocess_shell with proper handling
    return {
        "command": cmd,
        "cwd": str(cwd) if cwd else ".",
        "stdout": f"[STUB] Executed: {cmd}\n(Real subprocess + timeout + redaction coming next)",
        "exit_code": 0,
        "duration": 0.1,
    }


# Map for dispatcher in executor
TOOL_IMPLS = {
    "read_file": read_file_impl,
    "list_directory": list_directory_impl,
    "grep": grep_impl,
    "apply_patch": apply_patch_impl,
    "run_terminal": run_terminal_impl,
}
