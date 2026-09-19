# T046: Cross-platform CI repairs

**Status:** Development

Implementation and local verification are complete. Native Windows/macOS acceptance
remains pending through the repair pull request before merge into development.

## Problem

CI run 35339709145 for development commit 77aa781 failed Linux static checks,
seven Windows library tests, and the macOS native interactive smoke. Release
Artifacts succeeded independently, so its success does not prove CI acceptance.

## Goals and scope

Repair the redundant context-test formatting, Windows instruction scope resolution,
legacy reconciliation failure, terminal restoration test portability, and native
interactive smoke regressions. Preserve bounded instruction discovery, link rejection,
auditable workload reconciliation, explicit workspace consent, terminal restoration,
and credential-free native qualification.

Affected areas: `src/context/`, `src/tools/delegation.rs`, `src/tui/`, interactive
smoke scripts, focused regression tests, and Task entries only where needed.
No persistence schema changes, release publication changes, or new frameworks.

## Design and implementation plan

1. Remove the redundant formatting expression without changing test data.
2. Resolve valid Windows root aliases without weakening worktree containment or
   accepting linked paths; cover existing and prospective child paths.
3. Diagnose the legacy-audit reconciliation deadline failure and fix the demonstrated
   cause while retaining bounded locking and exactly-once audit adoption.
4. Separate pure terminal-sequence assertions from native console operations where
   necessary, retaining native Windows restoration verification.
5. Update native smoke input and assertions for current consent and interaction
   behavior. Preserve exact child status, bounded waits, persisted-state assertions,
   privacy checks, and terminal mode restoration.
6. Review spec compliance, then code quality; run local gates and record native
   platform limitations explicitly.

## Alternatives

Disabling warnings, skipping native tests, ignoring pipeline failures, and removing
consent would hide regressions. Reuse the existing filesystem and terminal abstractions
and repair fixtures or implementation at the demonstrated failing boundary.

## Acceptance criteria

- [x] Linux static checks pass with no new lint suppressions.
- [ ] Valid Windows instruction scopes resolve while outside-root and linked paths fail.
- [x] Legacy audit adoption retains one reconciliation event without lock failure.
- [ ] Terminal restoration tests work without assuming a Windows console in unit tests.
- [x] Offline interactive smokes exercise current consent and interaction behavior,
      validate successful child exit and exact terminal restoration, and remain bounded.
- [x] `task verify`, documentation checks, relevant focused checks, runtime coverage,
      and Linux release interactive smoke pass locally.
- [ ] Exact-revision hosted Windows and macOS CI pass before native closure.

## Validation gates

Use `task check`, `task test:agent-context`, `task test:interactive`, relevant
delegation regression tasks, `task verify`, `task docs:check`, `task coverage`, and
`task smoke:interactive`. Cross-check Windows compilation if the target is installed.
Hosted CI remains authoritative for native Windows and macOS execution.

## Risks and rollout

Path normalization is a containment boundary: retain component and file identity
validation. Reconciliation changes must preserve lock ordering and audit identity.
Smoke fixtures must test consent rather than globally bypassing it. Ship as a focused
repair and retain this spec in development until native evidence exists.

## Open questions

Native Windows and macOS execution is available through hosted CI; local Linux
verification alone cannot close native acceptance.

## Implementation findings (2026-09-19)

- Instruction discovery canonicalized the root but compared it lexically against
  caller scopes containing Windows verbatim-prefix or DOS-short-path aliases. Match
  only the root alias through the existing filesystem helper and retain child path
  spelling for containment and link checks.
- The legacy audit fixture's two-second deadline covered filesystem work and final
  identity readback, not just lock acquisition. The preceding hosted Windows run
  passed it in approximately 0.82 seconds. Align the fixture with the existing
  five-second production record-lock budget; retain production deadlines and test
  retry idempotency plus the existing contention/deadline regression.
- Crossterm can call native Windows console APIs even with an in-memory output
  writer. Pure ANSI encoding tests must not assume a live native console.
- The automatic TUI smoke sent two quit keystrokes. At the new workspace-consent
  prompt, the first immediately declined and exited, leaving the second write to
  fail with SIGPIPE on macOS. The smoke must explicitly accept and verify consent
  before exercising the rest of the interaction. Normal TUI quit still requires
  two keystrokes; emit them together once consent has been persisted.
- Plans now print and continue automatically. Replace obsolete plan-approval
  expectations with an isolated instruction policy requiring approval for
  `list_directory`, and verify question answers and action approval in persisted
  session evidence. No product permission behavior changes are needed.

## Validation evidence (2026-09-19)

- `task verify` passed strict formatting and all-target/all-feature Clippy, all 1,161
  library tests, all 86 CLI tests, every deterministic integration target, and
  doctests. Credentialed live qualification and the separately invoked exact-release
  harness remained explicitly ignored as designed.
- `task test:agent-context` passed 59 context, 73 agent/planner, and two build-metadata
  tests, including the existing long-instruction fixture and new containment/link
  regressions.
- `task test:delegation` passed the legacy-audit adoption/retry and held-modern-stripe
  single-deadline regressions, supporting persistence checks, all 37 managed-process
  tests, and all 22 delegation integration tests.
- Independent spec-compliance and context containment code-quality reviews found no
  blocking findings. Final independent terminal-smoke review likewise found no
  weakened success, approval, privacy, or restoration checks. These reviews do not
  substitute for native execution.
- `task docs:check` passed all five checks; `task installers:check` and
  `git diff --check` passed.
- `task build` and `task smoke:interactive:binary` passed. The full offline Linux
  PTY/redirected smoke also passed against the debug binary while the optimized
  build was being prepared. It exercised consent, action approval, questions,
  composer/history, queued work, steering, cancellation, resume, and privacy with
  successful child status and exact terminal restoration.
- `CARGO_BUILD_JOBS=2 task coverage` passed its complete instrumented suite and
  reported **85.84% runtime line coverage (112,201/130,706 lines)**, above the
  configured 80% gate. Reduced build parallelism bounded compiler memory use.
- Windows cross-target checking was attempted through
  `task check:all-targets TARGET=x86_64-pc-windows-msvc`, but failed in dependency
  compilation because the local MSVC librarian `lib.exe` is unavailable, before
  checking nib. Hosted Windows and macOS evidence remains open.

The checked local smoke criterion is supported by Linux execution. The updated
Windows smoke, Windows-only alias/junction regressions, and Windows console-free
encoding branch still require native CI. No production permission, terminal
restoration, persistence schema, or delegation timeout behavior was changed.
