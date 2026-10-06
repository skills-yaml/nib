# nib Workspace Docs Inventory

Metadata:

- Adopted standard: workspace-docs@7.0.0
- Status: current inventory
- Owner: project
- Last reviewed: 2026-10-06

## Adopted Files

- `AGENTS.md`
- `README.md`
- `workspace/instructions/tech/task.md`
- `workspace/instructions/tech/sdlc.md`
- `workspace/instructions/tech/project_structure.md`
- `workspace/instructions/tech/backend_rust.md`
- `workspace/instructions/tech/ci.md`
- `workspace/specs/README.md`
- `workspace/specs/backlog/`
- `workspace/specs/development/`
- `workspace/specs/done/`
- `workspace/instructions/standards/workspace-docs/ADOPTION.md`
- `workspace/agents/memory/README.md`
- `workspace/agents/memory/decisions.md`
- `workspace/agents/memory/facts.md`
- `workspace/agents/memory/preferences.md`
- `workspace/agents/memory/open-questions.md`
- `workspace/agents/memory/changelog.md`

## Current V7 Inventory

The current catalog contains 77 done specs (including 70 preserved historical
records), one development spec (T023) and two backlog proposals.
T061 and linked T067 are complete after verified main delivery on 2026-10-06.
T062 and linked T063/T064/T065/T066 are complete after verified main delivery. The complete standard,
DESIGN.md, blocked/test directories, feature catalog and native gates are adopted.
Older inventory counts below are historical snapshots, superseded by this section.

## Active Gaps and Future Scope

- T023 remains in `development/`. The protected six-provider catalog passed on
  2026-09-25 and reviewed OpenRouter IDs/retained canary reports are recorded.
  Tool-continuation failures remain open, including Anthropic final-response refusal;
  selected/full qualification has not passed. The 2026-10-01 decision retains the
  current Anthropic default and fixes continuation. Further paid runs require
  protected approvals, budgets and reviewed evidence.
- FT-020 is in `done/`. Protected Windows Job owner/DACL and macOS reaper preflight
  shipped; `production()` stays fail-closed on Windows and macOS. Current v1
  production delegation remains Linux+bwrap only until a later native qualification
  enables those platforms.
- MCP v1 is stdio-only; HTTP/SSE and OAuth require a separate future spec.
- Live paid-provider qualification remains an active external gate and
  remains explicitly authorization-bound. Completed Windows/macOS mechanism evidence
  does not enable production delegation there; FT-020 keeps those platforms
  fail-closed until a later native qualification record.

## Legacy Spec Paths (Aligned)

The product foundation is preserved under `workspace/specs/legacy/foundation/`.
Current specs use their single primary-feature directory under the canonical
lifecycle states. Old feature/task paths below are historical references only.

See `workspace/specs/README.md` for details. Canonical states are `backlog/`, `development/`, `test/`, `done/` and `blocked/`.

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

## Current T062 Delivery Evidence

Verified main merge `4b0245b9890bb15a4c99ff46ec7296d42bf9401f` through PR43 on
2026-10-05T20:20:23Z follows development integration
`30e9ecf8151768ca236367ed4984e1a06bc96380` through PR45. Exact-source CI
[37361857315](https://github.com/skills-yaml/nib/actions/runs/37361857315)
passed Linux, macOS and Windows full suites, binary qualification and native
smokes; development CI 37361857527 passed its Linux/macOS gates.
Frozen native stage `42550166c79df8d7acf4d0c405f65ce04d5947e8` passed ordered `task check` and
`task test` with required native bwrap, exactly composing `task verify`.
The new guard/owner cases execute in the full suites. Event/catalog/memory
reconciliation requires renewed exact review, focused documentation/version/
Workspace regression gates and frozen canonical verification before final delivery.
Publication is a separate event; the already-published shared version is 0.2.0.
No paid live qualification was performed by this migration.

## Current T061 Delivery Evidence

The same qualified source `a3b99632ae8cb452d5bfaff5555a3728999e8ba5` was delivered
to development on 2026-10-06 at 06:37:20 UTC and main at 06:40:26 UTC, with both
remote refs confirmed. Whole-spec independent acceptance, frozen native Task
verification and [CI 37419726793](https://github.com/skills-yaml/nib/actions/runs/37419726793)
passed, including Linux/macOS/Windows full suites, exact optimized-binary identity,
native interaction smokes and 84.48% Linux coverage. T061/T067 reconcile every
criterion, member paths and shipped memory; final records require renewed exact
review and verification before delivery. The single runtime target is 0.3.0;
T061's published 0.2.0 membership preserves contract-documentation history only.
No paid LLM qualification was performed. Public archive publication is separate.

## Historical 2026-09-02 Validation

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
- Lifecycle: 77 specs are in `done/`, T023 alone is in `development/`, and
  FT-021/FT-022 remain in `backlog/`. `workspace/specs/README.md` is the authoritative per-spec
  index.
- Question forms: legacy single calls and sets of up to eight share described choices,
  visible custom editors, proposal rows, whole-call discussion and interruption.
  Persisted obligations bind exact operation/plan/invocation identities; conversation
  can recover the linked operation without bypassing tool approval or verification.
  `/plan` and `/questions` are removed from interactive commands; runtime plan state remains.
- Project documentation: fixed local standards/library roots are loaded read-only with
  deterministic ordering, symlink rejection, traversal/file/byte caps, and aggregate
  model-context accounting.

## Notes

- Existing project-specific manual instructions remain outside generated `AGENT-CONTEXT` markers.
- Historical proposal content was preserved inside the canonical spec files and
  clearly labeled where current Rust behavior supersedes it.
- No secrets or environment-specific credential values were added.

## Historical 2026-06-19 Alignment

- Created/updated `workspace/specs/feature/ft_003_adopt_codex_sandboxing.md` (Symphony-style) describing **direct use of bwrap** inside nib's ToolExecutor for command sandboxing (Codex implementation used as reference for safe patterns and profiles). Preferred over full `codex sandbox` delegation for better integration.
- Added cross-references in FT-001 and architecture.md.

- Updated FT-001, FT-002, product.md, T001 with correct statuses, removed outdated "(to be created)", added Implementation Status sections, aligned tool descriptions and cross-refs to `workspace/instructions/tech/*` (architecture, permissions, etc.).
- Updated `workspace/specs/README.md` and this inventory to record the alignment.
- No file moves were performed in that historical pass. The later 2026-07-15 audit
  migrated lifecycle-managed specs into canonical state directories and supersedes
  those path/status claims.
