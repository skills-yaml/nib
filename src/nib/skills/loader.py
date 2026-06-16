"""Loading and activating skills at runtime."""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pathlib import Path

from .discovery import Skill, discover_skills


def load_skill(skill_path: Path) -> Skill:
    """Load a single skill by its SKILL.md path."""
    text = skill_path.read_text(encoding="utf-8")
    from .discovery import _parse_frontmatter  # local import to avoid cycle

    fm, body = _parse_frontmatter(text)
    name = fm.get("name") or skill_path.parent.name
    return Skill(name=name, path=skill_path, frontmatter=fm, body=body)


def activate_relevant_skills(
    project_root: Path | None = None,
    task_description: str | None = None,
) -> list[Skill]:
    """Heuristic activation of skills relevant to the current context.

    For now: discover everything + simple keyword matching on task description.
    Later: more sophisticated selection or user-configured activation.
    """
    skills = list(discover_skills(project_root=project_root))

    if not task_description:
        return skills

    query = task_description.lower()
    relevant = []
    for skill in skills:
        # Naive relevance: name or body mentions keywords from the task
        tags = skill.frontmatter.get("tags", []) or []
        if (
            skill.name.lower() in query
            or any(isinstance(tag, str) and tag.lower() in query for tag in tags)
            or (query.split() and query.split()[0] in skill.body.lower())
        ):
            relevant.append(skill)

    return relevant or skills  # fall back to all if nothing matched
