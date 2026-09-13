# Software Development Lifecycle (SDLC)

This document defines the development process and task organization for the nib project. It is intentionally lightweight and tailored to a small, focused local-first agent tool while still drawing from the high-quality patterns used across the workspace.

## Work Organization in nib

Two state models have separate purposes:

- **Repository development specs** use `docs/specs/backlog/`, `development/`, and `done/`. The only allowed transitions are `backlog -> development` and `development -> done`.
- **Runtime workload state** is profile-scoped session JSON plus durable daemon task
  records. A session contains a structured plan whose steps move through `Pending`,
  `InProgress`, `Blocked`, and `Completed`, plus lifecycle events and audited tool
  outcomes; detached and scheduled work adds persisted leases, cancellation, and
  reconciliation state.

nib does not currently ship a Projects/Tasks/Epics database or a Backlog/Working/Done board. Such a product domain requires a separate spec and migration design; repository spec folders must not be presented as runtime user data.

## Development Workflow

1. **Foundation & Specs first** — Major work begins with (or references) a document in `docs/specs/`:
   - Foundation product doc for overall direction.
   - Feature specs (`ft_XXX`) for new capabilities.
   - Task specs for granular implementation units, with an accompanying `.plan.md` when
     a separate generated execution plan is used.

2. **Spec Lifecycle (Backlog → Development → Done)**
   - New accepted ideas begin in `docs/specs/backlog/`.
   - A spec moves to `development/` only with scope, acceptance criteria, affected areas,
     an implementation or rollout plan, validation gates, and risks.
   - While in development, implementation uses the `ToolExecutor`, isolated worktrees where required, and two-stage review.
   - A spec only moves to `done/` after:
     - Execution (or delegation) is complete.
     - Reconciliation / verification has occurred (tests, diff review, workload model updated).
     - All relevant artifacts and decisions are recorded.
   - Blocked development remains in `development/` with the blocker recorded.

3. **Branching**
   - `main`: Production / stable.
   - `feature/<name>` or `feat/<slug>` for new work.
   - `fix/<slug>` for targeted fixes.
   - Short-lived task branches may be used under a feature when following plan-driven execution (fresh sub-agent per task).

4. **Pull Request / Change Process**
   - All changes go through review (human or structured agent review).
   - Use `task check` and focused tests for iterative feedback; the complete local
     quality gate is `task verify`.
   - For agent-driven work, follow subagent-driven-development patterns (fresh context + spec compliance review + quality review).
   - Update specs when behavior or interfaces change.
   - Moving a development spec to `done/` requires its acceptance evidence and canonical gates to be recorded.

5. **Quality Gates (minimum)**
   - `task verify` before completion
   - Linting + formatting
   - Type / static checking
   - Relevant tests
   - Self-review + (where applicable) agent review loops
   - Spec-state and runtime session consistency after reconciliation

6. **Agent-assisted development**
   - Prefer the available skills and subagent patterns in this environment (`symphony-spec-writing`, `subagent-driven-development`, `design`, `implement`, `review`, etc.).
   - Use worktree isolation for risky or parallel execution.
   - Always reconcile execution results back into the authoritative workload state.
   - The agent should only advance specs through the allowed state transitions when it has the necessary evidence.

See the more complete references in:
- `~/work/projects/agents/docs/tech/sdlc.md`
- `~/work/projects/revized/docs/tech/sdlc.md`

Runtime product expansion beyond session plans belongs in future feature specs rather than implied behavior in this process document.
