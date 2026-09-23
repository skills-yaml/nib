# T043: Oversized Modules and Coverage-Artifact Hygiene

**Status:** Backlog

**Related:** [Project Structure](../../tech/project_structure.md),
[Backend Rust](../../tech/backend_rust.md), [Task Runner](../../tech/task.md),
[SDLC](../../tech/sdlc.md),
[ft_015 Subagent Delegation](../done/ft_015_subagent_delegation.md),
[T002 Agent Framework](../done/T002_agent_framework_runtime_and_orchestration_engine.md),
[T035 Fast Incremental Check](../done/T035_fast_incremental_check_and_single_full_verification.md)

## Objective and Scope

Make the codebase reviewable again without changing runtime behavior, and
remove ~173 MB of regenerable coverage clutter from the working tree without
losing the 80% coverage gate.

Two tracks, both behavior-preserving:

- **Track A — Decompose god-files.** Split oversized modules into focused
  modules behind the existing public API so one file fits in one review pass
  and one lane context.
- **Track B — Coverage-artifact hygiene.** Delete the on-disk `.profraw`
  clutter, keep the coverage gate green, and stop recurrence.

Out of scope: behavior changes, new features, performance optimization,
coverage-threshold changes, paid qualification, releases.

The source audit is pinned to `433c42c`. Measured at that revision:
`src/tools/delegation.rs` 21,459 lines, `src/sandbox/worktree.rs` 10,521,
`src/tui/mod.rs` 9,085, `src/agent/loop.rs` 7,989,
`src/daemons/workload.rs` 7,914, `src/daemons/state.rs` 7,717,
`src/sandbox/process.rs` 7,427 — 155,403 Rust lines total.
120 `*.profraw` files totalling 173 MB sit in the repo root; zero are
git-tracked (`git ls-files | grep -c profraw` = 0) and the pattern is already
ignored by `.gitignore:46`, so this is working-tree clutter, not committed
history.

## Required Behavior

### Track A: module decomposition

1. No public-API breakage per slice. Each extraction keeps existing callers
   compiling through a thin re-export facade; call-site churn is a defect,
   not a cleanup.
2. One responsibility per new module. Candidate seams at the audited revision:
   - `tools/delegation.rs` → dispatch/policy vs. record/audit vs. fallback
     vs. tests.
   - `sandbox/worktree.rs`, `agent/loop.rs`, `daemons/workload.rs`,
     `daemons/state.rs`, `sandbox/process.rs`, `tui/mod.rs` → split only
     along seams found by inspection during development, one file per slice.
3. Size target: no shipped `src/` file exceeds 2,000 lines after the track,
   excluding generated code. Slices land smallest-first so every step stays
   reviewable by one lane.
4. Tests move with the code they cover. Coverage per moved area must not
   decrease; the workspace 80% gate stays green on every slice.
5. `docs/tech/project_structure.md` is updated when module ownership changes.

### Track B: coverage-artifact hygiene

1. Delete the 120 root `*.profraw` files (regenerable; zero tracked).
2. Keep `task coverage` green and output-contained: report stays at
   `target/` (already ignored); no new tracked or root-level artifact paths.
3. Document the rule in `docs/tech/task.md` or the coverage script help:
   coverage artifacts are reproduced by running the task, never stored.
4. Optional hardening (decide before development): set `LLVM_PROFILE_FILE`
   to a `target/` path in `scripts/check-runtime-coverage.sh` so future runs
   cannot re-clutter the root even if a developer overrides the profile.

## Acceptance Criteria

| ID | Scenario | Required evidence |
| --- | --- | --- |
| C01 | Full workspace builds after every slice. | `cargo check --all-targets --all-features` (or `task check:all-targets`) passes per slice. |
| C02 | No `src/` file over 2,000 lines. | `wc -l` listing of `src/` shows every file ≤ 2,000 lines, generated code called out or excluded by documented rule. |
| C03 | Public API stable per slice. | `git diff` per slice shows callers outside the split area untouched except re-export path updates; previously passing tests pass unmodified. |
| C04 | Coverage does not regress. | `task coverage` passes (≥ 80%) after Track A and after Track B. |
| C05 | Working tree clean of profraw. | `ls *.profraw` returns none; `du` shows 0; `git status --ignored --short` shows no root-level `!! *.profraw` entries. |
| C06 | Recurrence prevented. | Fresh `task coverage` run leaves zero `*.profraw` outside `target/`; documented rule exists where `task.md` says repeatable commands belong. |
| C07 | Docs match reality. | `project_structure.md` (and `task.md` if touched) diff reflects the new module/artifact layout; `task docs:check` passes. |

- [ ] C01–C04 pass per decomposition slice (no squash-and-pray mega-diff).
- [ ] C05–C07 pass for the hygiene track.
- [ ] Source review, two-stage review (spec compliance then quality), and
  required gates recorded before any move to `done/`.

## Affected Areas and Implementation Plan

Affected areas: `src/tools/delegation.rs`, `src/sandbox/worktree.rs`,
`src/tui/mod.rs`, `src/agent/loop.rs`, `src/daemons/workload.rs`,
`src/daemons/state.rs`, `src/sandbox/process.rs` (+ seams found on
inspection), `scripts/check-runtime-coverage.sh`, `docs/tech/task.md`,
`docs/tech/project_structure.md`, Task targets, regression fixtures.

1. Hygiene first (Track B): delete root `*.profraw`, run `task coverage`,
   confirm containment, document the rule. Small, unblocks everything else.
2. Map seams in the smallest god-file first; land one extraction slice with
   facade + moved tests + `task check` and focused tests.
3. Repeat largest-last (`delegation.rs` goes last — it is the highest-risk
   split). One spec slice per file or per seam, each independently
   reviewable.
4. Update `project_structure.md` as ownership changes; complete spec
   compliance review, then quality review, then gates.

Use child task specs with `.plan.md` files if a slice needs its own
execution plan. This backlog spec does not authorize starting runtime
implementation, publishing a release, or running paid qualification.

## Validation Gates

While implementing, use `task check` and focused `task test:*` targets per
slice (`test:delegation` for delegation slices at minimum). Completion
requires `task verify`, `task coverage`, `task docs:check`,
`git diff --check`, and the C01–C07 evidence above. All fixtures are
credential-free and bounded.

## Risks and Decisions Before Development

- Decide the per-slice order and the exact seam for the first extraction;
  do not start with `delegation.rs`.
- Decide whether `LLVM_PROFILE_FILE` hardening goes into
  `check-runtime-coverage.sh` or developer docs — root re-clutter after a
  manual `cargo test` with coverage env vars remains possible otherwise.
- Decide the generated-code exclusion rule for the 2,000-line target, if any
  file claims it.
- Splits must not smuggle behavior changes; any behavioral fix found during
  inspection gets its own spec/task, not a ride-along.
- Blocked development stays in `development/` with the blocker recorded;
  do not collapse slices into one unreviewable diff to "finish faster".
