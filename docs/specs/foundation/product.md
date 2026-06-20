# nib — Project Document

## Project description

nib is an AI agent specialized in coding and workload management. It acts as a persistent, opinionated partner that owns a personal (or small-team) development backlog and drives real progress by combining high-quality workload tracking with powerful code execution capabilities.

It maintains a living model of Projects, Work Items (tasks/epics with priorities, dependencies, estimates, status, and links), and execution context. nib can ingest goals from chat, GitHub issues, Notion, or free-form discussion; decompose them into actionable, reviewable plans; select the next best work; implement changes (directly or via disciplined delegation to sub-agents and specialized tools); verify results through tests, reviews, and reconciliation; and keep the workload state accurate and visible.

Target users are individual developers, technical founders, and small teams who want an always-available agent that not only writes code but also maintains clarity on what matters, what is blocked, and what should be done next. Key positioning: workload-native (the backlog is not an afterthought), execution-strong (uses worktrees, fresh subagents, verification loops, and the best available coding agents as lanes), and deeply integrated with the local development environment and external systems (Git, GitHub, Notion, existing agent platforms).

## Project vision

Become the most reliable personal "chief of staff + senior engineer" for software development — the agent that knows your projects better than any single session, keeps the workload truthful, surfaces the right decisions, and consistently ships high-quality increments with minimal context loss across days and weeks.

## Project mission

Deliver a focused agent (initially strong as a CLI/TUI companion, later with optional interfaces) that (1) maintains an accurate, queryable, and prioritizable workload model, (2) excels at turning goals into well-scoped, verifiable plans, (3) executes or orchestrates coding work using rigorous subagent-driven and lane-based patterns with mandatory verification, and (4) provides excellent visibility and control so a human can stay in the loop at the right moments — all while leveraging and enhancing the powerful agent ecosystems already present in the environment (skill systems, kanban/delegation patterns, Grok subagents, MCP integrations, etc.).

## Main features of the MVP

See also the detailed base architecture in [docs/tech/architecture.md](../tech/architecture.md), the permission model in [docs/tech/permissions.md](../tech/permissions.md), and ecosystem integration in [docs/tech/ecosystem_integration.md](../tech/ecosystem_integration.md).

* **Workload model & persistence** — Local-first durable store (Projects, Tasks/Epics, dependencies, priorities, estimates, status, history, links to external issues/PRs). Rich queries and "what next?" recommendations.
* **Goal intake & structuring** — Convert vague requests, tickets, or conversation into crisp work items with acceptance criteria, using symphony-style decomposition.
* **Planning & decomposition** — Produce (and maintain) clear implementation plans. Support for task breakdown that feeds directly into execution workflows.
* **Smart prioritization & selection** — Surface the highest-leverage next actions considering dependencies, risk, user context, and workload state.
* **Disciplined execution** — Direct edits or delegation (fresh subagents per task, two-stage spec + quality review, worktree isolation, Codex/Grok lanes and similar). Strong TDD and verification bias.
* **Reconciliation & truth maintenance** — Review diffs, run tests, update workload state, link artifacts (commits, PRs), record rationale and risks.
* **Visibility & control** — Excellent TUI/CLI views of the backlog, progress, blocked items, and agent activity. Human-in-the-loop escalation for decisions, clarifications, and approvals.
* **Integration bridges** — Deep use of GitHub (via MCP or skills), Notion, existing local agents (subagent profiles, sub-delegation), and the surrounding skill/registry system.
* **Persistent self** — Long-term memory of the user's projects, preferences, architecture decisions, and learned workflows that survives across sessions.

## Documentation & Process

This project follows the `workspace-docs@1.0.0` standard:

- Specs are organized under `docs/specs/` with states (`backlog/`, `development/`, `done/`) plus legacy reference paths (`feature/`, `foundation/`, `task/`) preserved during adoption.
- All non-trivial work is tied to specs (see `docs/specs/README.md`).
- Engineering conventions live in `docs/tech/` (project_structure, sdlc, backend_python, task runner, permissions, architecture, etc.).
- Authoritative guidance for contributors is in the root `AGENTS.md`.

See `docs/tech/` and `AGENTS.md` before making changes.