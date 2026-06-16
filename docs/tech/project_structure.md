# Project Structure

This document defines the layout and ownership boundaries for the nib project.

(Modeled on patterns from revized, autonomus, and the central reference in `~/work/projects/agents/docs/tech/project_structure.md`.)

## High-level layout (initial)

```
nib/
├── Taskfile.yml                 # Root task runner (mandatory)
├── AGENTS.md                    # Rules for AI agents and contributors
├── README.md
├── docs/
│   ├── specs/
│   │   ├── foundation/          # product.md and other foundational docs
│   │   ├── feature/             # ft_XXX feature specifications
│   │   └── task/                # TXXX task specs (+ .plan.md companions)
│   └── tech/                    # Architecture & process references
│       ├── project_structure.md
│       ├── sdlc.md
│       ├── task.md
│       └── ...
├── src/ or backend/             # (To be defined once architecture is chosen)
├── fe/ or ui/ or tui/           # (If a frontend/TUI is part of scope)
└── deployment/                  # Docker, Taskfiles, tf/ for infra (if applicable)
```

## Boundaries & Conventions

- `docs/specs/` is the source of truth for product intent. Never implement against a vague ticket without a corresponding spec or plan.
- Implementation code will follow the chosen runtime (likely Python/uv with src/ layout or a focused CLI/TUI package). See future backend_python.md or equivalent.
- All repeatable commands go through the root (or included) Taskfile.yml. Do not rely on ad-hoc shell one-liners for CI or agent workflows.
- External integrations (GitHub via MCP, Notion, Hermes skills/kanban, Grok subagents) are treated as adapters, not core domain.

See the central `agents/docs/tech/project_structure.md` and `agents/docs/tech/sdlc.md` for additional monorepo and layering guidance until nib-specific versions are expanded.

> **Note:** This is a placeholder. Expand this document once the runtime architecture (CLI-only vs full service, Python vs other, etc.) is decided.