# Base Architecture of nib

nib is a **local-first, persistent AI agent** specialized in **coding and workload management**. It owns a living model of projects and tasks, breaks down goals into verifiable plans, executes (or delegates) work through a safe, auditable tool surface, reconciles results, and keeps the user in the loop at the right moments.

This document describes the **base architecture** — the core components, data flows, principles, and integration points that every part of nib must respect.

See also:
- [Project Structure](project_structure.md)
- [Backend Python](backend_python.md)
- [Permissions](permissions.md) (defense-in-depth model)
- [Ecosystem Integration](ecosystem_integration.md) (MCP, Skills, AGENTS.md)
- [FT-001: Basic Agent Tools](../specs/feature/ft_001_basic_agent_tools.md)

## High-Level Principles

1. **Workload Model is Sacred**
   - The persistent store of Projects, Tasks (with status, priorities, dependencies, estimates, links), history, and execution records is the single source of truth.
   - Every planning, execution, and reconciliation step must ultimately update or query this model.
   - Tool calls and approvals are recorded against specific Tasks for auditability.

2. **Defense-in-Depth for Safety**
   - No tool action (especially destructive ones like `run_terminal` or broad patches) can occur without passing multiple independent layers: scoping, isolation (worktrees), classification, policy/AGENTS.md rules, explicit approval (manual or prior grant), redaction, and audit.
   - See the full [Permissions](permissions.md) document.

3. **Leverage, Don't Duplicate**
   - Reuse existing ecosystem primitives: kanban/todo/delegation/cron patterns, subagent patterns, MCP servers (GitHub, Notion, etc.), Skills (SKILL.md), AGENTS.md guidelines.
   - nib's job is **orchestration + workload truth + safe execution**, not reimplementing a general-purpose agent.

4. **Fresh Context + Verification Loops**
   - Prefer fresh sub-agents, isolated worktrees, and clean context for implementation work.
   - Two-stage review (spec compliance then quality) and post-execution reconciliation are default.
   - The agent (and sub-agents) must load and respect relevant AGENTS.md + active Skills before acting.

5. **Human-in-the-Loop by Default**
   - Status, blockers, decisions, and risks are highly visible (CLI + TUI).
   - Escalation points (clarify, approve, review diff) are first-class.

6. **Context-Rich but Token-Efficient**
   - Early in any task: assemble rich context (AGENTS.md, relevant Skills, project standards/libs docs, current workload snapshot, connected MCP tools).
   - Future work will address token budgets via summarization and selective loading.

## Core Components

```
User / Workload Owner
        │
        ▼
┌──────────────────────────────┐
│         CLI / TUI            │  (Typer + Textual; status, approvals, backlog views)
└──────────────┬───────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│              Context Loader                   │  (AGENTS.md walk-up, Skills discovery/activation,
│  (src/nib/context/)                           │   project standards, libs docs, MCP tool list)
└──────────────┬────────────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│              Planner                          │  (Goal intake → decomposition → structured plan
│  (core/planner.py - future)                   │   using symphony-style + skills)
└──────────────┬────────────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│         Tool Executor (the Gatekeeper)        │  (src/nib/tools/executor.py)
│  • Tool Registry (metadata + PermissionLevel) │
│  • Scoping + Worktree isolation               │
│  • Classification (read-only / safe /         │
│    destructive / network)                     │
│  • Policy + AGENTS.md + Skills constraints    │
│  • Approval workflow (manual/smart/policy/off)│
│  • Redaction                                  │
│  • Dispatch to impls + Audit to Workload      │
└──────────────┬────────────────────────────────┘
               │
       ┌───────┴───────┬───────────────┬──────────────┐
       ▼               ▼               ▼              ▼
┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
│ Core Tools  │ │ Integrations│ │ MCP Client  │ │ Sub-agents  │
│ (read_file, │ │ (git,       │ │ / Server    │ │ (subagent   │
│  list_dir,  │ │  subagent,  │ │ (expose     │ │  profiles,  │
│  grep,      │ │  lanes,     │ │  nib tools  │ │  lanes,     │
│  apply_patch,│ │  github...) │ │  to others) │ │  etc.)      │
│  run_terminal)│ └─────────────┘ └─────────────┘ └─────────────┘
└─────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│              Reconciler                       │  (Verify diffs, run tests, update workload state,
│  (core/reconciler.py - future)                │   link artifacts, record rationale)
└──────────────┬────────────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│           Workload Store (SQLite)             │  (aiosqlite + Pydantic models)
│  • Projects, Tasks, history, tool_executions, │
│    approvals, context snapshots               │
│  • Queries for "what next?", status, audit    │
└───────────────────────────────────────────────┘
```

### Key Modules (current + planned)

- `core/models.py` — Pydantic domain models (Project, Task, TaskStatus, Priority, ToolExecutionRecord, etc.)
- `core/workload.py` — Persistence, init tables, record_tool_execution, get snapshots/history
- `tools/` (new in this phase)
  - `models.py` — PermissionLevel, ApprovalMode, ToolCall/Result, ApprovalRequest/Decision, ToolExecutionRecord
  - `registry.py` — Static metadata registration
  - `executor.py` — The central gate (all layers)
  - `core_tools.py` — Implementations of the 5 minimal tools + dispatcher
  - `worktree.py` — Git worktree isolation manager (create_for_task, cleanup, status)
- `context/`
  - `agents.py` — Walk-up discovery + loading of AGENTS.md / CLAUDE.md
  - `loader.py` — assemble_context() + format for prompts (AGENTS + active Skills)
- `skills/`
  - Discovery of SKILL.md (standard locations + project-local)
  - Naive frontmatter parsing + activation heuristics
- `integrations/mcp.py` — ClientManager + MCPServer (dynamic exposure of registered tools via registry)
- `cli/` & `tui/` — Thin adapters over core (demo-tool currently exercises executor)
- `utils/` — Logging, redaction helpers (future)

## Data Flow for a Typical Task

1. **Intake / Activation**
   - User creates task or nib loads from backlog.
   - Context Loader walks for AGENTS.md, discovers/activates relevant Skills, pulls project standards + libs docs, lists MCP tools.

2. **Planning**
   - Planner receives rich context + goal.
   - Produces structured plan (tasks, dependencies, acceptance criteria, tool usage hints).
   - Plan is stored in workload.

3. **Execution (gated)**
   - For each step: build ToolCall.
   - ToolExecutor:
     - Resolves scope + worktree (if needed).
     - Classifies action.
     - Applies policy + AGENTS/Skills rules.
     - Triggers approval (if required) → records decision.
     - Dispatches to implementation (redacted args).
     - Records full ToolExecutionRecord (call + result + approval + context) to workload.
   - Sub-delegation (fresh context + worktree) when appropriate.

4. **Reconciliation**
   - Reconciler reviews diffs/artifacts, runs verification (tests/lint), updates Task status.
   - Surfaces risks or needed human input.

5. **Visibility**
   - CLI/TUI shows workload state, recent tool history (with approval sources), blocked items.

## Persistence

- Local SQLite (`~/.nib/workload.db`) for rich querying, relationships, and audit.
- Tables: projects, tasks, tool_executions (with JSON for args/output + approval metadata).
- Future: export/import, git-friendly snapshots, or optional bridges to Notion/GitHub Projects.

## Integration Points (Ecosystem)

- **AGENTS.md / CLAUDE.md**: Automatically discovered and injected. Rules can influence classification, require extra approvals, or define safe-mode allowlists.
- **Skills (SKILL.md)**: Discovered from standard locations in the ecosystem (e.g. ~/.grok/skills and project-local). Provide instructions, constraints, wrappers, or post-hooks. nib itself can be published as a skill.
- **MCP**: Client consumes external tools (GitHub, Notion, etc.). Server exposes nib tools (workload queries + safe executor calls) so other agents can delegate work to nib's permission model.
- **Sub-agents / Lanes**: Delegation targets with fresh context + worktree. nib owns the lifecycle and reconciliation.
- **Git**: Worktree isolation for changes; status/diff helpers.
- **Libs Documentation**: Read-only scoped access during context assembly so the agent understands domain boundaries (see previous requirements).

## Technology & Quality

(See [Backend Python](backend_python.md) for full details.)
- Python 3.12+, uv, src/ layout, ruff + pyright (strict), pytest-asyncio.
- Pydantic for all domain + tool models.
- aiosqlite for persistence.
- Typer + Rich for CLI; Textual for TUI.
- All repeatable work via Taskfile.

## Future Evolution

- Richer Planner (full symphony-style + multi-step reasoning).
- Advanced Context Management (token budgets, summarization).
- Smart Approval Classifier (rules + small model).
- MCP production client/server (with OAuth handling).
- Optional web dashboard or HTTP surface.
- Skill authoring tools and curator (following ecosystem patterns).
- Tighter integration with existing productivity skills (Linear, Notion, calendar).

This base architecture ensures nib remains a trustworthy "chief of staff + senior engineer" that owns your workload while never performing destructive actions without proper consent.

Update this document whenever core components, flows, or principles change.