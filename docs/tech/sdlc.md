# Software Development Lifecycle (SDLC)

(Placeholder modeled on revized / autonomus / central agents docs.)

## Development Workflow

1. **Foundation & Specs first** — Major work begins with (or references) a document in `docs/specs/`:
   - Foundation product doc for overall direction.
   - Feature specs (`ft_XXX`) for new capabilities.
   - Task specs + `.plan.md` for granular implementation units.

2. **Branching**
   - `main`: Production / stable.
   - `feature/<name>` or `feat/<slug>` for new work.
   - `fix/<slug>` for targeted fixes.
   - Task branches may be short-lived under a feature when using plan-driven execution.

3. **Pull Request / Change Process**
   - All changes go through review (human or structured agent review).
   - Quality gates must pass (`task check`, `task test`).
   - For agent-driven work, follow subagent-driven-development patterns (fresh context + spec compliance review + quality review).
   - Update specs when behavior or interfaces change.

4. **Quality Gates (minimum)**
   - Linting + formatting
   - Type / static checking
   - Relevant tests
   - Self-review + (where applicable) agent review loops
   - Workload model consistency (when the change affects tasks or execution state)

5. **Agent-assisted development**
   - Prefer the available skills and subagent patterns in this environment (`symphony-spec-writing`, `subagent-driven-development`, `design`, `implement`, `review`, etc.).
   - Use worktree isolation for risky or parallel execution.
   - Always reconcile execution results back into the authoritative workload state.

See the more complete references in:
- `~/work/projects/agents/docs/tech/sdlc.md`
- `~/work/projects/revized/docs/tech/sdlc.md`

This document will be expanded as nib defines its own release and review processes.