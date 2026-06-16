# Task Runner (Taskfile)

nib uses [Task](https://taskfile.dev/) as the standard interface for all local and CI operations. This mirrors the convention across the workspace (revized, autonomus, skm, flirtyr, etc.).

## Rules

- Every repeatable command that a human or agent would run belongs in a Taskfile.
- Root `Taskfile.yml` is the entry point. Use `includes:` for subprojects (backend, fe, deployment, etc.) when they exist.
- Agents and CI must invoke tasks rather than raw commands (e.g. `task check`, `task test`, not direct `ruff` or `pytest`).
- Keep task names stable and descriptive (`check`, `test`, `fmt`, `build`, `deploy`, `coverage:report`, scoped variants like `backend:check`).

## Current minimal tasks (see root Taskfile.yml)

- `task` or `task default` — list tasks
- `task check` — linting, formatting, static checks (expand as implementation begins)
- `task test` — run test suites
- `task fmt` — auto-format

## Adding new tasks

When you introduce a new build, test, or automation step, add a corresponding task entry (and update any sub-Taskfiles). Document the task briefly in its `desc:` and `summary:` fields.

Reference implementations:
- `~/work/projects/skm/Taskfile.yml` (simple single-binary)
- `~/work/projects/revized/Taskfile.yml` (with includes for fe/backend/deployment)
- Central guidance in `~/work/projects/agents/docs/tech/task.md`

This is a starting placeholder. Flesh it out as soon as concrete implementation directories and pipelines are added.