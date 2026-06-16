"""Skills system integration (SKILL.md discovery and loading).

nib participates in the same skill ecosystem used by Hermes and the Grok TUI.
"""

from .discovery import Skill, discover_skills
from .loader import load_skill

__all__ = ["Skill", "discover_skills", "load_skill"]
