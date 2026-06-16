"""Skill discovery (SKILL.md files).

Follows the structure defined in ~/work/projects/registry/SKILL_STRUCTURE.md
and used by skm + Hermes.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Iterator


@dataclass
class Skill:
    name: str
    path: Path
    frontmatter: dict[str, Any]
    body: str


SKILL_FILENAME = "SKILL.md"

# Common locations to search (user can extend via config later)
DEFAULT_SEARCH_PATHS = [
    Path.home() / ".grok" / "skills",
    Path.home() / ".hermes" / "skills",
    Path.home() / ".config" / "nib" / "skills",
]


def _parse_frontmatter(text: str) -> tuple[dict[str, Any], str]:
    """Very simple YAML frontmatter parser for --- ... --- blocks."""
    match = re.match(r"^---\s*\n(.*?)\n---\s*\n(.*)$", text, re.DOTALL)
    if not match:
        return {}, text

    raw_fm, body = match.groups()
    frontmatter: dict = {}
    for line in raw_fm.strip().splitlines():
        if ":" in line:
            key, _, val = line.partition(":")
            key = key.strip()
            val = val.strip().strip('"').strip("'")
            # Very naive list support
            if val.startswith("[") and val.endswith("]"):
                val = [v.strip().strip('"').strip("'") for v in val[1:-1].split(",") if v.strip()]
            frontmatter[key] = val
    return frontmatter, body.strip()


def discover_skills(
    extra_paths: list[Path] | None = None,
    project_root: Path | None = None,
) -> Iterator[Skill]:
    """Yield discovered Skill objects from standard and extra locations.

    If project_root is given, also looks for local skills under it.
    """
    search_roots = list(DEFAULT_SEARCH_PATHS)
    if extra_paths:
        search_roots.extend(extra_paths)
    if project_root:
        search_roots.append(project_root / "skills")
        search_roots.append(project_root / ".nib" / "skills")

    seen: set[Path] = set()

    for root in search_roots:
        if not root.exists():
            continue
        for skill_md in root.rglob(SKILL_FILENAME):
            if skill_md in seen:
                continue
            seen.add(skill_md)
            try:
                text = skill_md.read_text(encoding="utf-8")
                fm, body = _parse_frontmatter(text)
                name = fm.get("name") or skill_md.parent.name
                yield Skill(name=name, path=skill_md, frontmatter=fm, body=body)
            except Exception:
                # Skip unreadable or malformed skills gracefully
                continue
