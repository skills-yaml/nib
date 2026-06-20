"""High-level context assembly for nib.

When working on a task or project, nib assembles rich context from:
- AGENTS.md files
- Activated skills
- Connected MCP tools (future)
- Project standards / libs docs (see previous design)

This context is then injected into planning, execution, and the TUI.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from nib.context.agents import load_agents_md
from nib.skills.loader import activate_relevant_skills

if TYPE_CHECKING:
    from pathlib import Path


def assemble_context(
    project_path: Path | None = None,
    task_description: str | None = None,
    include_agents_md: bool = True,
    activate_skills: bool = True,
) -> dict[str, Any]:
    """Build a rich context dictionary for the current work.

    This should be called early in planning or when a task becomes active.
    """
    context: dict[str, Any] = {
        "project_path": str(project_path) if project_path else None,
        "task_description": task_description,
    }

    if include_agents_md and project_path:
        context["agents_md"] = load_agents_md(project_path)

    if activate_skills:
        skills = activate_relevant_skills(
            project_root=project_path,
            task_description=task_description,
        )
        context["active_skills"] = [
            {
                "name": s.name,
                "frontmatter": s.frontmatter,
                "body_preview": s.body[:500] + "..." if len(s.body) > 500 else s.body,
            }
            for s in skills
        ]
        context["skills_instructions"] = "\n\n".join(
            f"## Skill: {s.name}\n{s.body}"
            for s in skills[:3]  # limit to avoid token bloat
        )

    # TODO: MCP tools summary, libs docs, recent session history, etc.

    return context


def format_context_for_prompt(context: dict[str, Any]) -> str:
    """Turn the assembled context into a prompt-friendly block."""
    parts = []

    if agents := context.get("agents_md"):
        parts.append("## Project Agent Guidelines (AGENTS.md)\n" + agents[:3000])

    if skills_text := context.get("skills_instructions"):
        parts.append("## Relevant Skills\n" + skills_text)

    if task := context.get("task_description"):
        parts.append(f"## Current Task\n{task}")

    return "\n\n".join(parts)
