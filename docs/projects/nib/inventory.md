# nib Workspace Docs Inventory

Metadata:

- Adopted standard: workspace-docs@1.2.0
- Status: current inventory
- Owner: project
- Last reviewed: 2026-09-02

## Adopted Files

- `AGENTS.md`
- `README.md`
- `docs/tech/task.md`
- `docs/tech/sdlc.md`
- `docs/tech/project_structure.md`
- `docs/tech/backend_rust.md`
- `docs/tech/ci.md`
- `docs/specs/README.md`
- `docs/specs/backlog/`
- `docs/specs/development/`
- `docs/specs/done/`
- `docs/standards/workspace-docs/README.md`
- `agents/memory/README.md`
- `agents/memory/decisions.md`
- `agents/memory/facts.md`
- `agents/memory/preferences.md`
- `agents/memory/open-questions.md`
- `agents/memory/changelog.md`

## Active Gaps and Future Scope

- T023 is in `development/`. Its credential-free harness is locally green, but live
  catalog/canary/selected/full evidence requires owner-approved credentials, budgets,
  OpenRouter exact IDs, and protected-workflow authority.
- FT-020 is in `backlog/` for a future protected cleanup-authority design that could
  enable production delegation on Windows or macOS. Current v1 production delegation
  remains Linux+bwrap only.
- `docs/specs/feature/` and `docs/specs/task/` are retained only as empty legacy
  directories; active lifecycle state uses the canonical state directories.
- MCP v1 is stdio-only; HTTP/SSE and OAuth require a separate future spec.
- Live paid-provider qualification is the only active implementation-spec gate and
  remains explicitly authorization-bound. Completed Windows/macOS mechanism evidence
  does not enable production delegation there; those platforms require FT-020 or another
  approved protected-authority design.

## Legacy Spec Paths (Aligned)

The product foundation remains under `foundation/`. Feature and task specs have been
migrated from the legacy `feature/` and `task/` paths into `development/` or `done/`
so their lifecycle state is explicit.

See `docs/specs/README.md` for details. Canonical states (`backlog/`, `development/`, `done/`) are now preferred for future specs.

## Quality Gates Available

- `task --list`
- `task check`
- `task test`
- `task verify`
- `task docs:check`
- `task check:all-targets`
- `task coverage`
- `task build`
- `task qualify:llm-release`
- `task smoke:interactive`
- `task smoke:managed-process`

## Current Validation Run

On 2026-09-02, local `task verify` passed 1,062 library tests, 86 CLI tests,
every integration suite, and doctests. Exact hosted run
[`33683995100`](https://github.com/skills-yaml/nib/actions/runs/33683995100)
then passed Validate, macOS Tests, and Windows Tests for head
`c3b88564da4f6f654a8618e4fa544b353ece86f5` at clean merge checkout
`0479b72ad3d11fd7221632f042736b8489b6443b`. It included native all-target
checks, complete serial suites, 85.87 percent Linux runtime line coverage
(102,061/118,862), exact release-binary qualification, Linux/macOS PTY and redirected
smokes, Windows ConPTY/`TERM=dumb`/redirected smoke, and Linux abrupt-owner containment.
The explicit paid live-provider entrypoint remained ignored; no provider credential was
read and no paid request was made.

## Reconciled Runtime Inventory

- Persistence: `.nib/profiles/<id>/sessions/*.json` stores messages, structured
  `PlanStep` state, lifecycle events, and audited tool calls. Profile daemon JSON
  stores durable background and scheduled task records with leases and reconciliation.
- Execution ownership: one OS-backed lease covers an entire active run for a session.
  Structured plans carry an immutable ID and normalized goal; stale approval or tool
  outcomes cannot mutate a replacement plan, and completed plans cannot authorize
  further mutations.
- Audit fallback: executor calls without an operational session persist redacted
  attempts and outcomes in a profile-scoped implicit audit session. That session is
  evidence only and never becomes schedule, background-work, or plan authority.
- Superseded design: nib does not ship the historical SQLite/global Projects, Tasks,
  Epics, or Backlog database proposed in T002.
- MCP: the v1 outbound client and inbound server use stdio. HTTP/SSE and OAuth are
  historical/future T006 ideas, not shipped behavior.
- External chat: provider adapters own authentication, listeners, and replies; nib's
  boundary is the normalized gateway in `src/integrations/gateway.rs`.
- Lifecycle: 44 specs are in `done/`, T023 is the sole `development/` spec, and FT-020
  is the sole `backlog/` spec. `docs/specs/README.md` is the authoritative per-spec
  index.
- Project documentation: fixed local standards/library roots are loaded read-only with
  deterministic ordering, symlink rejection, traversal/file/byte caps, and aggregate
  model-context accounting.

## Notes

- Existing project-specific manual instructions remain outside generated `AGENT-CONTEXT` markers.
- Historical proposal content was preserved inside the canonical spec files and
  clearly labeled where current Rust behavior supersedes it.
- No secrets or environment-specific credential values were added.

## Historical 2026-06-19 Alignment

- Created/updated `docs/specs/feature/ft_003_adopt_codex_sandboxing.md` (Symphony-style) describing **direct use of bwrap** inside nib's ToolExecutor for command sandboxing (Codex implementation used as reference for safe patterns and profiles). Preferred over full `codex sandbox` delegation for better integration.
- Added cross-references in FT-001 and architecture.md.

- Updated FT-001, FT-002, product.md, T001 with correct statuses, removed outdated "(to be created)", added Implementation Status sections, aligned tool descriptions and cross-refs to `docs/tech/*` (architecture, permissions, etc.).
- Updated `docs/specs/README.md` and this inventory to record the alignment.
- No file moves were performed in that historical pass. The later 2026-07-15 audit
  migrated lifecycle-managed specs into canonical state directories and supersedes
  those path/status claims.
