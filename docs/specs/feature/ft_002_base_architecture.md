# FT-002: Base Architecture of the Agent

**Status:** Implemented (architecture.md created + core modules aligned)  
**Related:** [Product Foundation](../foundation/product.md), [FT-001: Basic Agent Tools](../feature/ft_001_basic_agent_tools.md), [T001](../task/T001_implement_core_agent_tools.md)

## Overview

This feature defines the **base architecture** for nib — the high-level components, data flows, principles, and integration points required for a trustworthy, local-first AI agent that owns coding workload and executes safely.

It codifies the lessons from the permissions deep-dive, ecosystem requirements (MCP, Skills, AGENTS.md), tool executor implementation, and workspace conventions.

## Goals

- Establish a clear, documented mental model that all future code, specs, and contributors must follow.
- Ensure the architecture enforces "workload model is sacred", defense-in-depth permissions, fresh context + verification, and leverage of the existing ecosystem.
- Provide a single reference (this spec + the detailed `docs/tech/architecture.md`) so implementation of Planner, Reconciler, richer TUI, MCP production support, etc. stays consistent.
- Make the architecture visible and reviewable (text diagrams, principles, flow).

## Non-goals (for this feature)

- Full implementation of every box in the diagram (Planner and Reconciler are still skeletal).
- Web UI, compiled distribution, or heavy external frameworks.
- Complete token-budget solution or smart approval classifier (those are follow-up issues).

## Base Architecture Principles

1. **Workload Model is Sacred** — Every significant action must update or query the persistent Projects/Tasks/execution/approval records.
2. **Defense-in-Depth for Safety** — Multiple layers (scoping, worktrees, classification, policy/AGENTS/Skills, explicit approval, redaction, audit) must all pass for destructive actions. No single "yolo" switch bypasses everything.
3. **Leverage, Don't Duplicate** — Heavily reuse subagent patterns, MCP servers, Skills (SKILL.md), and AGENTS.md guidelines from the surrounding ecosystem.
4. **Fresh Context + Verification Loops** — Prefer isolated worktrees + clean sub-agents. Always load AGENTS.md + relevant Skills before planning or tool use. Reconciliation is mandatory.
5. **Human-in-the-Loop by Default** — Status, risks, and approvals are first-class in CLI/TUI.
6. **Context-Rich but Bounded** — Early context assembly (AGENTS, Skills, standards, workload snapshot, MCP tools) is required, but future work must address token budgets.

## Core Components & Data Flow

See the detailed component diagram and flow in [docs/tech/architecture.md](../../tech/architecture.md).

High-level flow for a task:
1. Activation → Context Loader (AGENTS.md + Skills + standards + MCP tools)
2. Planning (receives rich context)
3. Execution through central ToolExecutor (all permission layers + dispatch + audit)
4. Reconciliation (verify + update workload)
5. Visibility (CLI/TUI shows history with approval sources)

## Key Modules (as-built + planned)

- `core/` — models, workload (persistence + audit), planner (future), executor (via tools/), reconciler (future)
- `tools/` — models (PermissionLevel, Approval*, Tool*), registry, executor (the gatekeeper), core_tools, worktree
- `context/` — agents (AGENTS.md loader), loader (assemble + format)
- `skills/` — discovery + activation
- `integrations/` — mcp (client + dynamic server), git, subagent, lanes
- `cli/` + `tui/` — thin surfaces

## Technology Choices

See [docs/tech/backend_python.md](../../tech/backend_python.md) for the full stack and quality rules.

## Integration Requirements

- AGENTS.md / Skills / MCP must be first-class in context assembly and must influence ToolExecutor decisions.
- Tool calls and approvals are always recorded against a Task.
- Destructive actions require explicit approval or prior grant (see FT-001 and permissions doc).

## Acceptance Criteria

- `docs/tech/architecture.md` exists and is referenced from project_structure.md, backend_python.md, ecosystem_integration.md, permissions.md, and the product foundation.
- A feature spec (this document) exists that clearly states the principles and high-level flow.
- The current code (ToolExecutor + permission layers, context loader, workload recording + tool audit table, worktree manager, MCP server stubs, skills discovery) matches the described architecture (see architecture.md "as-built" sections and FT-001 implementation status).
- Diagrams and module ownership are kept up-to-date.
- Future features (Planner, full TUI approvals, production MCP, etc.) will have their own specs that reference this base architecture.

## Open Questions / Follow-ups

- Detailed token budget / context management strategy (see issue #6)
- Sophistication of smart approval classifier (see issue #7)
- Exact patch format + search/replace alternative (see FT-001 open questions)
- Full integration of Planner and Reconciler components
- Production-grade MCP client/server with OAuth and error handling

Update this spec and the architecture document whenever the core model changes.