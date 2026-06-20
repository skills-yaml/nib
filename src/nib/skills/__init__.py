"""Skills system integration (SKILL.md discovery and loading).

nib participates in the skill ecosystem used across the Grok TUI and similar tools in the workspace.
"""

from .discovery import Skill, discover_skills
from .loader import load_skill

__all__ = ["Skill", "discover_skills", "load_skill"]
