# nib — Project Document

## Project description

nib is a local-first AI agent specialized in coding and workload execution. It turns a goal into a persisted, reviewable plan, runs approved tools in scoped worktrees and optional `bwrap` isolation, and reconciles every run into a profile-scoped session record.

The shipped workload model is the session: indexed messages, a structured plan and step outcomes, lifecycle events, skill usage, memory, and audited tool executions. Goals enter through the CLI, chat, TUI, stdio MCP, or an external adapter using nib's normalized gateway contract. GitHub/Notion intake and a richer cross-project Projects/Tasks/Epics domain are possible bridges, not current built-in behavior.

Target users are individual developers, technical founders, and small teams who want an auditable coding agent that preserves context between runs and makes approval, execution, and verification state visible. Key positioning: session-workload native, execution-strong, local-first, and interoperable through Skills and MCP.

## Project vision

Become a reliable persistent senior-engineer agent that can resume work without losing the plan, keep execution history truthful, surface decisions, and ship verified increments with human control at the right boundaries.

## Project mission

Deliver a focused CLI/TUI agent that (1) persists accurate session, plan, memory, and execution state, (2) turns goals into well-scoped, verifiable plans, (3) executes or delegates coding work through the same permission and audit boundaries, and (4) keeps clarification, approval, verification, and reconciliation visible to a human.

## Main features of the MVP

See also the detailed base architecture in [docs/tech/architecture.md](../../tech/architecture.md), the permission model in [docs/tech/permissions.md](../../tech/permissions.md), and ecosystem integration in [docs/tech/ecosystem_integration.md](../../tech/ecosystem_integration.md).

* **Workload model & persistence** — Profile-scoped JSON sessions containing indexed messages, structured plan steps, lifecycle events, tool records, skill usage, and reconciliation outcomes.
* **Goal intake & structuring** — Convert a CLI, chat, TUI, MCP, or normalized gateway request into a persisted Symphony-style plan.
* **Planning & decomposition** — Maintain approved plan steps and update their status from observed tool and model outcomes.
* **Disciplined execution** — Direct edits or delegation (fresh subagents per task, two-stage spec + quality review, worktree isolation, Codex/Grok lanes and similar). Strong TDD and verification bias.
* **Reconciliation & truth maintenance** — Preserve results, stderr, approvals, boundaries, worktrees, plan links, and final run outcomes in the originating session.
* **Visibility & control** — CLI/TUI views of session history and live model/tool state, with explicit plan/tool approvals and clarification prompts.
* **Integration bridges** — Stdio MCP client/server support, normalized external gateway adapters, structured Skills, and isolated subagent worktrees.
* **Persistent context** — Profile-specific environment/user memory plus bounded session summarization that preserves the raw audit trail.

A global prioritization engine, backlog boards, issue/PR synchronization, and native provider listeners remain roadmap work and require their own development specs.

## Documentation & Process

This project follows the repository-pinned `workspace-docs@1.2.0` standard:

- Specs are organized under `docs/specs/` with states (`backlog/`, `development/`, `done/`) plus legacy reference paths (`feature/`, `foundation/`, `task/`) preserved during adoption.
- All non-trivial work is tied to specs (see `docs/specs/README.md`).
- Engineering conventions live in `docs/tech/` (project structure, SDLC, backend Rust, Task runner, permissions, architecture, and CI).
- Authoritative guidance for contributors is in the root `AGENTS.md`.

See `docs/tech/` and `AGENTS.md` before making changes.
