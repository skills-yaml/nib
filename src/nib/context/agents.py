"""AGENTS.md (and CLAUDE.md) discovery and loading.

nib must respect project-specific agent guidelines.
"""

from __future__ import annotations

from pathlib import Path

AGENTS_FILENAMES = ["AGENTS.md", "CLAUDE.md", "CLAUDE.local.md", "AGENTS.local.md"]


def find_agents_md(start_path: Path) -> Path | None:
    """Walk upwards from start_path looking for an AGENTS.md or equivalent.

    Returns the first one found, or None.
    """
    current = start_path.resolve()
    if current.is_file():
        current = current.parent

    root = Path("/")
    while current != root:
        for name in AGENTS_FILENAMES:
            candidate = current / name
            if candidate.is_file():
                return candidate
        current = current.parent

    # Also check home-level or central references if desired
    home_agents = Path.home() / "AGENTS.md"
    if home_agents.is_file():
        return home_agents

    return None


def load_agents_md(project_path: Path) -> str:
    """Load the most relevant AGENTS.md content for the given project.

    Returns the raw markdown content (or a helpful message if none found).
    """
    agents_path = find_agents_md(project_path)
    if agents_path is None:
        return (
            "# No AGENTS.md found\n\n"
            "nib is operating without project-specific agent guidelines.\n"
            "Consider creating an AGENTS.md in the project root."
        )

    try:
        content = agents_path.read_text(encoding="utf-8")
        header = f"# Loaded from {agents_path}\n\n"
        return header + content
    except Exception as e:
        return f"# Error loading {agents_path}\n\n{e!s}"
