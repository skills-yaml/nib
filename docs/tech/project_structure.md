# Project Structure

nib CLI is implemented in Rust (following skm structure) with Python core/agent logic (transitional).

See skm's project_structure.md for reference on CI/install layout.
Rust sources in src/ (main.rs, auth.rs, chat.rs, config.rs, agent/, llm/, etc).
Python sources remain in src/nib/ for agent/llm until ported.

## High-Level Layout

```
nib/
├── pyproject.toml
├── uv.lock
├── Taskfile.yml                 # All dev, check, test, and build commands
├── .python-version
├── AGENTS.md
├── README.md
├── .gitignore
├── docs/
│   ├── specs/
│   │   ├── foundation/
│   │   │   └── product.md
│   │   ├── feature/             # ft_XXX_*.md
│   │   └── task/                # TXXX_*.md (+ .plan.md)
│   └── tech/
│       ├── architecture.md      # Base architecture of the agent (components, flows, principles)
│       ├── backend_python.md    # nib-specific Python conventions (CLI/TUI agent)
│       ├── project_structure.md
│       ├── permissions.md       # Defense-in-depth permission model
│       ├── ecosystem_integration.md  # MCP, Skills, AGENTS.md
│       ├── sdlc.md
│       ├── task.md
│       └── cli_tui.md           # (future) detailed TUI/CLI guidelines
├── src/
│   └── nib/
│       ├── __init__.py
│       ├── __main__.py
│       ├── cli/                 # Typer command surface
│       ├── tui/                 # Textual interactive application
│       ├── core/                # Domain logic (sessions, planning, execution, reconciliation)
│       ├── integrations/        # Git, subagent, lanes, GitHub, Notion, etc.
│       ├── models/              # Pydantic data models (or keep in core)
│       └── utils/
└── tests/
```

## Key Directories & Ownership

- `src/nib/core/` — The heart of the agent. Workload model, planning, execution strategies, verification/reconciliation. This should be the most testable and framework-light part of the system.
- `src/nib/cli/` and `src/nib/tui/` — Presentation layers. They should stay relatively thin and delegate to `core/`.
- `src/nib/integrations/` — All external system interactions (spawning other agents, git operations, MCP/CLI bridges). Isolate these for easier testing and swapping.
- `docs/specs/` — Product truth. Never implement major behavior without a corresponding spec or task plan.
- `docs/tech/` — Engineering conventions. Keep them up to date as the project evolves.

## Python Package Rules

- Use the `src/` layout.
- All imports are absolute from the `nib` package: `from nib.core.workload import ...`
- The command `nib` is provided via the `[project.scripts]` entry point in `pyproject.toml`.
- Support `python -m nib` via `__main__.py`.

## What nib Is NOT (for structure decisions)

- Not a microservices platform → no `backend/libs/`, `srv/`, `lambda/`, Firestore, Pub/Sub, etc.
- Not primarily an API server (though it may grow lightweight MCP server or HTTP surfaces later).
- Primary interfaces are excellent **CLI + TUI**, not web UIs.

## Future Growth

If nib evolves to have:
- A web dashboard → add a `fe/` directory following the workspace frontend patterns.
- Background services or a small API → consider a `srv/` directory at that time.
- Distribution as a compiled binary → evaluate a thin Rust wrapper around the Python core (or full Rust rewrite).

For the MVP and early versions, keep it a clean, single `src/nib/` Python package with strong separation between `core/`, `integrations/`, and UI layers.

## References

- `docs/tech/architecture.md` — Base architecture, core components, data flows, and principles.
- `docs/tech/backend_python.md` — Detailed Python, CLI, TUI, and persistence conventions for nib.
- `docs/specs/feature/ft_002_base_architecture.md` — Feature spec for this architecture.
- Central workspace references (when more detail is needed):
  - `~/work/projects/agents/docs/tech/backend_python.md`
  - `~/work/projects/agents/docs/tech/project_structure.md`
  - `~/work/projects/agents/docs/tech/sdlc.md`

Update this document whenever the top-level layout changes significantly.
