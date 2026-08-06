# Task Runner (Taskfile)

nib uses [Task](https://taskfile.dev/) as the standard interface for all local and CI operations. This mirrors the convention across the workspace (revized, autonomus, skm, flirtyr, etc.).

## Rules

- Every repeatable command that a human or agent would run belongs in a Taskfile.
- Root `Taskfile.yml` is the entry point. Use `includes:` for subprojects (backend, fe, deployment, etc.) when they exist.
- Agents and CI must invoke tasks rather than raw commands (e.g. `task check`, `task test`, not direct `ruff` or `pytest`).
- Keep task names stable and descriptive (`check`, `test`, `fmt`, `build`, `deploy`, `coverage:report`, scoped variants like `backend:check`).

## Current minimal tasks (see root Taskfile.yml)

- `task` or `task default` — list tasks
- `task check` — installer checks, Rust formatting, Clippy, compilation, and the full serial test suite
- `task check:all-targets` — type-check every Rust target and feature (optionally for `TARGET`)
- `task fmt` — format Rust source
- `task test` — run the full Rust unit and integration suite serially
- `task test:durable` — run detached background-task and scheduled-worker process tests
- `task test:managed-process-capability` — verify the exact managed-process backend probe independently
- `task test:updater` — run self-update and update-notification unit tests
- `task qualify:release-update:unix` — on a native Linux/macOS release runner, install
  a supplied development bootstrap artifact and prove notice, replacement, and no-op
- `task qualify:release-update:windows` — run the equivalent qualification against a
  supplied development bootstrap artifact on a native Windows release runner
- `task test:installers` — run installer and release-transaction integration tests
- `task docs:check` — validate internal links, unique spec IDs, and done-spec acceptance state
- `task coverage` — enforce the configured runtime line-coverage threshold
- `task build` — build the locked optimized release binary (optionally for `TARGET`)
- `task smoke:managed-process` — build the Linux release binary, kill its active owner,
  and verify a detached supervised descendant is reaped before terminal publication
- `task fix` — apply Rust formatting and Clippy fixes
- `task installers:check` — validate installer syntax, repository defaults, and checksum logic

## Adding new tasks

When you introduce a new build, test, or automation step, add a corresponding task entry (and update any sub-Taskfiles). Document the task briefly in its `desc:` and `summary:` fields.

Reference implementations:
- `~/work/projects/skm/Taskfile.yml` (simple single-binary)
- `~/work/projects/revized/Taskfile.yml` (with includes for fe/backend/deployment)
- Central guidance in `~/work/projects/agents/docs/tech/task.md`

The root Taskfile is authoritative; update this list whenever a canonical task changes.
