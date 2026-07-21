# FT-002: Base Architecture of the Agent

**Status:** Done
**Related:** [Product Foundation](../foundation/product.md), [FT-001: Basic Agent Tools](ft_001_basic_agent_tools.md), [T001](T001_implement_core_agent_tools.md)

## Overview

This feature defines the **base architecture** for nib — the high-level components, data flows, principles, and integration points required for a trustworthy, local-first AI agent that owns coding workload and executes safely.

It codifies the lessons from the permissions deep-dive, ecosystem requirements (MCP, Skills, AGENTS.md), tool executor implementation, and workspace conventions.

> Historical baseline: The goals, module sketch, and follow-ups before the 2026-07-15
> reconciliation describe the pre-Rust architecture proposal. The current module and
> ownership truth is the Implementation Reconciliation below and
> `docs/tech/architecture.md`.

## Goals

- Establish a clear, documented mental model that all future code, specs, and contributors must follow.
- Ensure the architecture enforces "workload model is sacred", defense-in-depth permissions, fresh context + verification, and leverage of the existing ecosystem.
- Provide a single reference (this spec + the detailed `docs/tech/architecture.md`) so implementation of Planner, Reconciler, richer TUI, MCP production support, etc. stays consistent.
- Make the architecture visible and reviewable (text diagrams, principles, flow).

## Non-goals (for this feature)

- Full implementation of every box in the original diagram; Planner and Reconciler
  were still skeletal when this baseline was written.
- Web UI, compiled distribution, or heavy external frameworks.
- A complete token-budget solution or smart approval classifier; those were follow-up
  issues at the time and are reconciled by later feature specs.

## Base Architecture Principles

1. **Workload Model is Sacred** — Every significant action must update or query the persistent Projects/Tasks/execution/approval records.
2. **Defense-in-Depth for Safety** — Multiple layers (scoping, worktrees, classification, policy/AGENTS/Skills, explicit approval, redaction, audit) must all pass for destructive actions. No single "yolo" switch bypasses everything.
3. **Leverage, Don't Duplicate** — Heavily reuse subagent patterns, MCP servers, Skills (SKILL.md), and AGENTS.md guidelines from the surrounding ecosystem.
4. **Fresh Context + Verification Loops** — Prefer isolated worktrees + clean sub-agents. Always load AGENTS.md + relevant Skills before planning or tool use. Reconciliation is mandatory.
5. **Human-in-the-Loop by Default** — Status, risks, and approvals are first-class in CLI/TUI.
6. **Context-Rich but Bounded** — Early context assembly (AGENTS, Skills, standards,
   workload snapshot, MCP tools) is required; later work added aggregate token budgets.

## Core Components & Data Flow

See the detailed component diagram and flow in [docs/tech/architecture.md](../../tech/architecture.md).

High-level flow for a task:
1. Activation → Context Loader (AGENTS.md + Skills + standards + MCP tools)
2. Planning (receives rich context)
3. Execution through central ToolExecutor (all permission layers + dispatch + audit)
4. Reconciliation (verify + update workload)
5. Visibility (CLI/TUI shows history with approval sources)

## Historical Module Sketch (pre-Rust)

- `core/` — models, workload (persistence + audit), planner (future), executor (via tools/), reconciler (future)
- `tools/` — models (PermissionLevel, Approval*, Tool*), registry, executor (the gatekeeper), core_tools, worktree
- `context/` — agents (AGENTS.md loader), loader (assemble + format)
- `skills/` — discovery + activation
- `integrations/` — mcp (client + dynamic server), git, subagent, lanes
- `cli/` + `tui/` — thin surfaces

## Technology Choices

See [docs/tech/backend_rust.md](../../tech/backend_rust.md) for the full stack and quality rules.

## Integration Requirements

- AGENTS.md / Skills / MCP must be first-class in context assembly and must influence ToolExecutor decisions.
- The original model recorded calls and approvals against a global Task. The shipped
  model records them against profile sessions, plans, and durable task records.
- Destructive actions require explicit approval or prior grant (see FT-001 and permissions doc).

## Historical Acceptance Criteria

- `docs/tech/architecture.md` exists and is referenced from project_structure.md, backend_rust.md, ecosystem_integration.md, permissions.md, and the product foundation.
- A feature spec (this document) exists that clearly states the principles and high-level flow.
- The then-current code (ToolExecutor + permission layers, context loader, workload
  recording + tool audit table, worktree manager, MCP server stubs, skills discovery)
  matched the baseline architecture.
- Diagrams and module ownership are kept up-to-date.
- Subsequent Planner, TUI approval, and MCP features received their own specs that
  reference this base architecture.

## Historical Questions / Follow-ups

- Detailed token budget / context management strategy (see issue #6)
- Sophistication of smart approval classifier (see issue #7)
- Exact patch format + search/replace alternative (see FT-001 open questions)
- Full integration of Planner and Reconciler components
- Production-grade MCP client/server with OAuth and error handling

Update this spec and the architecture document whenever the core model changes.

## Reopened Audit (2026-07-15)

Scope: reconcile module ownership, links, diagrams, and future/as-built labels with
the Rust implementation produced by the reopened runtime specs.

Affected areas: `docs/tech/`, foundational specs, and architecture link validation.

Validation gates: `task docs:check`, architecture-to-source review, `task check`,
and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Keep the architecture reference aligned with the shipped Rust modules, profile-scoped
persistence, bounded loop, gated execution, stdio MCP, and external-adapter gateway boundary.

### Acceptance Criteria

- [x] `docs/tech/architecture.md` maps every runtime module and its data flow.
- [x] Architecture preserves authoritative workload updates, defense in depth, bounded context, and reconciliation.
- [x] Project structure and ecosystem docs agree with the Rust-only tree.
- [x] Final documentation and repository gates are green.

### Affected Areas

`docs/tech/architecture.md`, `docs/tech/project_structure.md`,
`docs/tech/ecosystem_integration.md`, foundational specs, and link validation.

### Implementation Evidence

The module map in `docs/tech/architecture.md` corresponds to `src/agent`, `context`,
`daemons`, `integrations`, `profile`, `sandbox`, `session`, `tools`, and `tui`.

### Validation Evidence

`tests/docs_integrity.rs` validates links and lifecycle state. Runtime conformance is
exercised by `tests/test_runtime_e2e.rs::runtime_sequence_selects_profile_context_and_skill_then_reconciles_audited_tools`.
`task docs:check` passed all five documentation integrity tests on 2026-07-15.

### Validation Gates

- [x] `task docs:check` after reconciliation (5 passed on 2026-07-15).
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Historical `core/` and future planner/reconciler wording remains design history; the
current Rust module map is authoritative. No in-scope architecture gap remains.

## Reopened Audit (2026-07-16)

### Scope

Restore the architecture's universal audit invariant and make its module ownership map
complete for the current library, binary, sandbox, and integration modules.

### Acceptance Criteria

- [x] Every production tool dispatch is session-audited or fails closed before execution.
- [x] `docs/tech/architecture.md` maps all active `src/lib.rs` and `src/main.rs` modules,
  including filesystem security, CLI commands, MCP framing, and managed-process backends.
- [x] The data-flow diagram distinguishes user-facing command surfaces from runtime
  ownership modules without implying unaudited direct execution.
- [x] Documentation integrity and architecture-to-source checks are green.

### Affected Areas

`src/tools/executor.rs`, `src/doctor.rs`, `docs/tech/architecture.md`,
`docs/tech/project_structure.md`, and documentation tests.

### Implementation Plan

1. Restore mandatory session audit at the central executor boundary.
2. Compare the public library and binary module declarations with the architecture map.
3. Separate presentation/command modules from runtime ownership in the documented flow.
4. Validate links, module inventory, and repository behavior together.

### Risks

Hand-maintained module maps can drift. The map now follows `src/lib.rs` and
`src/main.rs` directly, while documentation integrity and future architecture reviews
remain required when module declarations change.

### Completion Evidence

The central executor creates an implicit audit session or fails before dispatch.
`docs/tech/architecture.md` and `docs/tech/project_structure.md` enumerate the active
library, binary, CLI, filesystem-security, MCP-framing, sandbox-process, worktree, and
Windows Job modules, with command surfaces separated from runtime ownership.

### Validation Gates

Sessionless audit tests, source/module inventory comparison, `task docs:check`,
`task check`, and `task test`.
