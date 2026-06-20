# Software Development Lifecycle (SDLC)

This document defines the development process and task organization for the nib project. It is intentionally lightweight and tailored to a small, focused local-first agent tool while still drawing from the high-quality patterns used across the workspace.

## Task Organization in nib

nib itself is a workload management agent. For both **developing nib** and for the tasks that nib manages on behalf of its users, we organize work into three primary high-level buckets (visible in the TUI, CLI, and workload model):

- **Backlog** — Ideas, requirements, and planned work that is not yet active. This is the "to-do" queue. Tasks here have low or medium priority and no active execution.
- **Working** — Tasks that are actively being worked on or are ready to be worked on. This includes:
  - In progress
  - Blocked (awaiting input, review, external dependency, or user approval)
  - Under review or reconciliation
- **Done** — Completed, verified, and reconciled work. These tasks have a recorded outcome, any relevant artifacts (diffs, test results, approvals), and are considered closed for the current cycle.

### Why three buckets?

- **Simplicity** — A classic kanban-style board (Backlog → Working → Done) is easy for both humans and the agent to reason about.
- **Visibility** — The TUI and CLI surface the current "Working" set prominently so the user always knows what is in flight.
- **Agent discipline** — nib (and any sub-agents it spawns) should only pull work from the Backlog into Working when capacity exists, and must drive items through to Done with proper verification and recording.
- **Auditability** — Every transition between buckets (especially into Working and into Done) is recorded in the workload store together with context (AGENTS.md, active skills, approvals, tool usage).

### Internal Statuses vs. High-Level Buckets

While the workload model supports finer-grained `TaskStatus` values (`TODO`, `IN_PROGRESS`, `BLOCKED`, `DONE`, `CANCELLED`), the primary user- and agent-facing organization is the three-bucket model above.

- `Backlog` ≈ tasks whose high-level state is not yet active (`TODO` + low/medium priority, or explicitly parked).
- `Working` ≈ `IN_PROGRESS` + `BLOCKED`.
- `Done` ≈ `DONE` (and archived `CANCELLED` items when desired).

The agent should prefer to keep the "Working" bucket small and focused (one or two items at a time for the main loop, plus delegated sub-work in isolated worktrees).

## Development Workflow

1. **Foundation & Specs first** — Major work begins with (or references) a document in `docs/specs/`:
   - Foundation product doc for overall direction.
   - Feature specs (`ft_XXX`) for new capabilities.
   - Task specs + `.plan.md` for granular implementation units.

2. **Task Lifecycle (Backlog → Working → Done)**
   - New ideas or requirements are created in the **Backlog**.
   - When capacity exists and the task is ready (clear acceptance criteria, any needed context/AGENTS.md loaded), it is moved into **Working**.
   - While in Working the task may use the full tool surface (through the `ToolExecutor`), sub-delegation, worktree isolation, and approval flows.
   - A task only moves to **Done** after:
     - Execution (or delegation) is complete.
     - Reconciliation / verification has occurred (tests, diff review, workload model updated).
     - All relevant artifacts and decisions are recorded.
   - Blocked items stay in the Working bucket but surface clear escalation points for the user.

3. **Branching**
   - `main`: Production / stable.
   - `feature/<name>` or `feat/<slug>` for new work.
   - `fix/<slug>` for targeted fixes.
   - Short-lived task branches may be used under a feature when following plan-driven execution (fresh sub-agent per task).

4. **Pull Request / Change Process**
   - All changes go through review (human or structured agent review).
   - Quality gates must pass (`task check`, `task test`).
   - For agent-driven work, follow subagent-driven-development patterns (fresh context + spec compliance review + quality review).
   - Update specs when behavior or interfaces change.
   - Moving a development task from Working to Done requires that the associated PR (if any) is merged and the workload record reflects completion.

5. **Quality Gates (minimum)**
   - Linting + formatting
   - Type / static checking
   - Relevant tests
   - Self-review + (where applicable) agent review loops
   - Workload model consistency (task moved only after proper reconciliation and recording)

6. **Agent-assisted development**
   - Prefer the available skills and subagent patterns in this environment (`symphony-spec-writing`, `subagent-driven-development`, `design`, `implement`, `review`, etc.).
   - Use worktree isolation for risky or parallel execution.
   - Always reconcile execution results back into the authoritative workload state.
   - The agent should only advance tasks through the Backlog → Working → Done flow when it has the necessary context and permissions.

See the more complete references in:
- `~/work/projects/agents/docs/tech/sdlc.md`
- `~/work/projects/revized/docs/tech/sdlc.md`

This document will be expanded as nib defines its own release processes and as the TUI/kanban views mature. The three-bucket model (Backlog / Working / Done) is the primary way both humans and the agent itself should think about workload organization.