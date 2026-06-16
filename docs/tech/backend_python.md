# Backend Python (nib)

This document defines the Python standards for the nib project.

nib is a **local-first CLI + TUI AI agent** for coding and workload management, not a cloud SaaS backend. The standards here are therefore lighter and more focused on agent orchestration, terminal UX, local persistence, and fast iteration than the patterns used in `autonomus`/`revized`-style services.

## Technology Stack

- **Language**: Python 3.12+
- **Dependency Management**: `uv` (pyproject.toml + uv.lock)
- **CLI**: Typer (with Rich for output)
- **TUI**: Textual (for interactive workload views, kanban, live updates)
- **Models**: Pydantic (v2)
- **Persistence**: SQLite via `aiosqlite` (local-first workload store)
- **Quality**:
  - `ruff` (lint + format)
  - `pyright` (strict type checking)
- **Testing**: `pytest` + `pytest-asyncio`
- **Task Runner**: Taskfile (all checks, tests, and dev workflows go through `task`)

## Project Layout (Python-specific)

```txt
src/nib/
├── __init__.py
├── __main__.py          # python -m nib support
├── cli/
│   ├── __init__.py
│   └── app.py           # Typer application + command groups
├── tui/
│   ├── __init__.py
│   └── app.py           # Textual application entry
├── core/
│   ├── models.py        # Pydantic domain models (Project, Task, etc.)
│   ├── workload.py      # Persistence, queries, transactions
│   ├── planner.py       # Goal decomposition & plan generation
│   ├── executor.py      # Direct execution + delegation logic
│   └── reconciler.py    # Diff review, test verification, state updates
├── integrations/
│   ├── git.py
│   ├── hermes.py        # Spawning / communicating with Hermes profiles
│   ├── lanes.py         # Codex lanes, subagent delegation helpers
│   ├── mcp.py           # MCP client + server (core ecosystem integration)
│   └── github.py        # (via gh CLI or MCP)
├── context/             # Loaders for AGENTS.md, project standards, skills activation
│   └── ...
├── skills/              # Skill discovery, parsing (SKILL.md), and runtime activation
│   └── ...
├── models/              # Lightweight data shapes (if separating from core)
└── utils/
    └── logging.py
```

- Keep the public surface small.
- Core agent logic lives in `core/`.
- All I/O boundaries (git, subprocess, external agents) live in `integrations/`.
- TUI and CLI are thin adapters over the core.

## uv & Packaging Rules

- Single source of truth is `pyproject.toml` (PEP 621).
- Always commit `uv.lock`.
- Use `src/` layout. Imports must be absolute from the package root (`from nib.core.models import ...`).
- Install with `uv sync` (dev) or `uv sync --no-dev`.
- The `nib` command is provided via `[project.scripts]`.

**Never** use `requirements.txt`, ad-hoc venvs, or `pip install -e .` outside of uv.

## Code Quality & Tooling

Run **everything** through the root Taskfile:

```bash
task fmt      # ruff format
task lint     # ruff check
task check    # fmt + lint + pyright
task test     # pytest
```

- Use `ruff` for formatting and linting (configured in pyproject.toml).
- Use `pyright` in strict mode.
- All new behavior must have tests.
- Prefer `pydantic` models for data that crosses boundaries.

## Architecture Principles for nib

See also:
- [Ecosystem Integration](ecosystem_integration.md) for MCP, Skills, and AGENTS.md requirements.
- [Permissions](permissions.md) — the authoritative deep dive on preventing destructive actions without explicit approval or policy.

1. **Workload model is sacred**
   - The `core/workload.py` + Pydantic models are the single source of truth for projects/tasks/status.
   - Every execution path (direct edit or delegated) must eventually reconcile back into the workload store.

2. **Fresh context for execution**
   - When delegating implementation work, prefer spawning fresh agents (subagents, worktrees, Codex lanes) with clean context.
   - The orchestrator (nib) owns planning, selection, verification, and state updates.

3. **Human-in-the-loop by default**
   - The TUI/CLI should make status, blockers, and decisions highly visible.
   - Escalation points (clarify, approve, review diff) are first-class.

4. **Leverage, don't duplicate**
   - Reuse Hermes kanban/todo/delegation/cron where they are the right primitive.
   - Deeply integrate with the local ecosystem: MCP servers, the Skills (SKILL.md) system, and AGENTS.md files.
   - See the dedicated [Ecosystem Integration](ecosystem_integration.md) document for requirements and design.
   - nib's job is **orchestration + workload truth**, not being another general agent.

5. **Local-first persistence**
   - SQLite for rich querying and relationships.
   - Keep the schema simple and migratable.
   - Consider export/import for backup and sharing plans.

6. **Testability of agent flows**
   - Core logic (planner, executor, reconciler) should be testable with minimal I/O.
   - Use dependency injection or clear ports for integrations (git, agent spawners, etc.).
   - Deterministic tests for state transitions.

## Dependencies Policy

- Add dependencies deliberately. Every added dependency increases the surface that must be understood and secured.
- Prefer stdlib + small, high-quality libraries (Pydantic, Typer, Textual, Rich, aiosqlite).
- Heavy agent frameworks (LangGraph, CrewAI, AutoGen, etc.) are **not** used by default. Build the orchestration loops explicitly first so we understand the exact semantics we need.

## When to Update This Document

Update this document when we introduce:
- New primary UI patterns (new TUI framework, major CLI restructuring)
- Significant changes to the persistence model
- New conventions for delegation / verification loops
- Changes to the dev workflow (new required tools in Taskfile)

Keep this document practical and specific to building a reliable local coding + workload agent.

---

See also:
- `docs/tech/project_structure.md` (overall layout)
- `docs/tech/task.md` (Taskfile expectations)
- Sibling projects for inspiration: `autonomus` (orchestration patterns), `agents/skills/autonomous-ai-agents/hermes-agent` (real-world agent integration patterns)
