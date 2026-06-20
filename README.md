# nib

The AI agent that owns your coding workload and ships.

## Description

nib is a specialized AI agent for coding and workload management. It maintains a persistent, accurate model of your projects and work items while driving actual implementation through disciplined execution loops (planning, delegation, verification, and reconciliation).

Unlike general chat agents that forget context between sessions, nib treats the backlog as a first-class, long-lived artifact. It can break down goals, choose what to work on next, implement (or orchestrate) changes using the best available sub-agents and tools, verify the results, and keep the workload state truthful and visible.

Primary users:
- Individual developers and technical founders who want a reliable daily partner for both "what should I do?" and "do the work".
- Small teams that want consistent, reviewable agent-driven progress on code projects.
- Power users already living in rich agent environments (Grok subagents, MCPs, and similar tools) who want a workload-native orchestrator on top.

Key capabilities (MVP focus):
- Local-first workload tracking with Projects, Tasks, dependencies, priorities, and external links.
- High-quality planning using structured (Symphony-style) decomposition.
- Rigorous execution: fresh-context subagents, spec compliance reviews, quality reviews, isolated worktrees, and lane-based specialists (e.g. Codex lanes).
- Excellent TUI/CLI visibility and human steering.
- Strong integration with Git, GitHub (MCP), Notion, and existing agent platforms in the environment.

## Vision

The most trustworthy, persistent agent for solo and small-team software development — one that keeps your workload real, surfaces the right decisions, and reliably turns intent into shipped, verified code across days and weeks.

## Project Structure

- `docs/specs/` – Product and feature specifications (foundation, feature, and task docs following revized-style conventions).
- `docs/tech/` – Architecture and process references (to be populated).
- Implementation is in **Python** (uv + pyproject.toml, src/ layout).
- See `docs/tech/backend_python.md` and `docs/tech/project_structure.md` for the exact conventions and layout.

## Documentation Map

- [docs/specs/foundation/product.md](docs/specs/foundation/product.md) — **Product foundation document**. High-level description, vision, mission, and MVP feature outline.
- `docs/specs/feature/` — Future feature specifications (ft_XXX).
- `docs/specs/task/` — Task-level specs and associated .plan.md files.
- `docs/tech/` — Technical standards (project structure, SDLC, task runner, backend, etc.) — modeled on patterns from revized, autonomus, agents, and other projects.

## Getting Started (Early)

This project is in the early definition phase. The first artifact is the foundation product document.

Use the standard task and agent workflows once implementation begins:
- Follow patterns from `~/work/projects/agents/docs/tech/`
- Prefer Taskfile.yml for all repeatable commands.
- Use available skills (symphony-spec-writing, design, implement, review, subagent-driven-development, etc.) for agent-assisted work.

Run `task --list` (once a Taskfile exists) for available commands.