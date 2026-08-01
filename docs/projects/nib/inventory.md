# nib Workspace Docs Inventory

Metadata:

- Adopted standard: workspace-docs@1.2.0
- Status: current inventory
- Owner: project
- Last reviewed: 2026-07-29

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

- T003, T004, and T007 remain in `development/`: namespace quarantine and identity
  verification do not prove exact physical unlink on Unix, and their platform gates
  remain open.
- T006 remains in `development/` pending its own hosted Windows evidence reconciliation.
  T010 remains there pending an exact-current committed development-channel run and
  inspection of its published artifacts; rolling Releases now use the documented
  exclusive-writer operating contract.
- FT-015 remains in `development/` for Windows/macOS worktree runtime validation and
  the inherited FT-017 authority boundary.
- T020 and FT-016 have complete local stdio MCP lifecycle, cancellation, redaction, and
  metadata coverage. T020 remains open for Windows containment and macOS runtime
  validation; FT-016 remains open for Windows containment; both inherit FT-015's
  managed-worktree ownership limits.
- FT-017 is in `development/` and owns durable abrupt-owner descendant-process
  containment. Linux production proof, supervisor-crash recovery, launch fencing, and
  bounded scope retirement are integrated; Windows/macOS production delegation remains
  rejected until cleanup authority is inaccessible to managed workers.
- T021 is in `development/` and specifies explicit OpenAI-compatible API-mode and
  reasoning configuration plus Responses function-tool support.
- T022 is in `development/` and specifies the typed provider-neutral LLM contract,
  distinct provider adapters, native correlated tool continuation, safe terminal/error
  normalization, and shared provider conformance gates.
- `docs/specs/feature/` and `docs/specs/task/` are retained only as empty legacy
  directories; active lifecycle state uses the canonical state directories.
- MCP v1 is stdio-only; HTTP/SSE and OAuth require a separate future spec.
- Live paid-provider smoke remains optional operator validation. Windows and macOS
  runtime, containment, and file-identity gates are required by their owning development
  specs. Their current process backends are native mechanism evidence rather than an
  enabled production delegation contract.

## Legacy Spec Paths (Aligned)

The product foundation remains under `foundation/`. Feature and task specs have been
migrated from the legacy `feature/` and `task/` paths into `development/` or `done/`
so their lifecycle state is explicit.

See `docs/specs/README.md` for details. Canonical states (`backlog/`, `development/`, `done/`) are now preferred for future specs.

## Quality Gates Available

- `task --list`
- `task check`
- `task test`
- `task docs:check`
- `task coverage`
- `task build`

## Current Validation Run

On 2026-07-16, the reconciled Linux tree passed `task check` and an independent
`task test` with 795 top-level tests (601 library, 61 CLI, and 133 integration tests).
`task coverage` reported 83.90 percent runtime line coverage (55,083/65,656),
`task docs:check` passed all five invariants, and the locked optimized `task build`
completed. Strict all-target check/Clippy, format, and diff-whitespace gates also passed.

The locally built optimized release binary passed isolated help/version, healthy/failing doctor, skill
install/list/remove, outbound and inbound MCP initialize/list/call/error and size-bound
checks, durable cancellation/reconciliation and scheduled wake, and bounded project-doc
context loading. Linux raw-PTY runs passed plan denial, question selection,
destructive-tool denial, cancellation reconciliation, and session-detail open/close.
Windows runtime, Job Object, and reparse behavior and macOS runtime behavior were not
executed on this host and remain explicit development gates.
Local cross-target compilation is also blocked before nib code by the missing MSVC
`lib.exe` tool and the absence of an Apple-compatible C compiler and SDK.

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
- Lifecycle: 19 audited specs are in `done/`; T003, T004, T006, T007, T010, T020,
  FT-015, FT-016, FT-017, T021, and T022 are in `development/`; no spec is in
  `backlog/`.
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
