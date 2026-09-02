# T035: Fast Incremental Check and Single Full Verification

Status: Development

## Summary

Reduce local verification latency without weakening the completion gate. `task check`
becomes the fast static feedback loop, `task test` remains the authoritative full
serial Rust suite, and a new `task verify` composes both exactly once for local
completion. Development and CI guidance must stop treating `task check` as a second
full test run.

## Scope

- Keep installer syntax/default validation, Rust formatting, and warning-denying
  Clippy in `task check`.
- Remove the redundant default `cargo check` and the full serial test suite from
  `task check`.
- Keep `task test` as the unchanged full serial unit and integration suite.
- Add `task verify` as the canonical local aggregate of `task check` followed by
  `task test` exactly once.
- Make `task dev` use the aggregate before its optimized build and help smoke.
- Align repository instructions and technical documentation with the fast-feedback
  versus completion-verification distinction.
- Add deterministic Taskfile contract coverage for the composition and non-duplication
  rules.

## Non-Goals

- Parallelizing tests or changing isolation assumptions in the current serial suite.
- Splitting, deleting, ignoring, or weakening existing Rust tests.
- Changing coverage, release, smoke, live-provider, or cross-target qualification.
- Cleaning the existing Cargo target directory or changing Cargo cache/profile policy.
- Adding a new test runner, build cache, or dependency.

## Acceptance Criteria

- [x] `task check` runs installer validation, `cargo fmt -- --check`, and
      warning-denying Clippy, but does not run `cargo check` or `cargo test`.
- [x] `task test` continues to run the full Rust suite with `--test-threads=1`.
- [x] `task verify` runs `task check` and `task test` once each and performs no other
      duplicate Rust compile or test command.
- [x] `task dev` invokes `task verify` once before the release build and help smoke.
- [x] Linux CI retains one fast check step and one full test step; completion guidance
      names `task verify` as the single local aggregate while preserving focused tests
      during iteration.
- [x] `task test:task-contract` fails if tests or a redundant default `cargo check`
      return directly or through a nested task to `task check`, or if `verify`/`dev`
      composition drifts.
- [x] Task and SDLC documentation describe the new boundaries consistently.
- [x] Focused Task contract tests, `task docs:check`, `task check`, and
      `git diff --check` pass; full-suite status is reported truthfully against the
      current working tree.
- [ ] The exact committed revision passes hosted CI before T035 moves to `done/`.

## Affected Areas

- `Taskfile.yml` — fast check, aggregate verify, and non-duplicating dev composition.
- `tests/installers.rs` — static Taskfile composition regression.
- `AGENTS.md` — iterative versus completion gate guidance.
- `docs/tech/task.md` — authoritative task definitions.
- `docs/tech/sdlc.md` — development and completion workflow.
- `docs/tech/backend_rust.md` — Rust gate summary.
- `docs/tech/ci.md` — local development command semantics.
- `docs/specs/README.md` — lifecycle inventory.

## Implementation Plan

1. Add a static Taskfile contract test that extracts the `check`, `test`, `verify`,
   and `dev` sections and asserts their exact responsibilities.
2. Refactor Task composition while retaining installer, formatting, Clippy, serial
   test, build, and help behavior in their intended gates.
3. Update agent, Task, SDLC, Rust, and CI documentation to distinguish fast feedback
   from complete verification.
4. Run focused contract/documentation/static gates, inspect the diff, and record any
   full-suite limitation caused by unrelated working-tree state.

## Validation Gates

1. `task test:task-contract` validates the static Taskfile contract; the broader
   `task test:installers` suite retains its release and installer coverage.
2. `task docs:check` validates links and lifecycle state.
3. `task check` validates installer syntax, formatting, and Clippy without executing
   the serial suite.
4. `task verify` is the canonical complete local aggregate when the working tree is
   ready for full-suite execution.
5. `git diff --check` validates patch hygiene.

## Risks and Mitigations

- **Callers assume `task check` includes tests:** Update every authoritative workflow
  document and provide `task verify` as the explicit completion command.
- **A compile error escapes the fast gate:** Clippy compiles the default Rust targets,
  while `task test` compiles test targets in the completion aggregate.
- **Task composition regresses into duplicate work:** Keep string-level contract tests
  for command absence and exact nested-task counts.
- **Serial test safety is accidentally weakened:** Leave `task test` and all focused
  serial tasks unchanged.

## Implementation Reconciliation (2026-08-29)

The Task graph now separates fast feedback from full verification. `task check` runs
installer validation, formatting, and warning-denying Clippy only. `task test` retains
the unchanged serial suite, `task verify` composes the two once each, and `task dev`
uses that aggregate before its build and help smoke. Repository instructions and
technical docs describe the same boundary, and `task test:task-contract` statically
guards the composition.

Local evidence passed the dedicated Task contract test, all five documentation
integrity tests, and `git diff --check`. The previous check path took 244.47 seconds
before failing in the first serial test binary; the new path reached actionable Clippy
feedback in 25.53 seconds on the same host. The broader installer run passed its 39
pre-existing tests; its newly added contract test initially exposed and then received
a fix for YAML section parsing, after which the dedicated regression passed.

The initially reported shared-tree Clippy finding in `src/tools/delegation.rs` was
reconciled as part of the owning runtime repair. On 2026-09-02,
`task test:task-contract`, `task test:installers` (40/40), `task docs:check` (5/5),
`task check`, and `git diff --check` passed. The canonical `task verify` completed the
static gate once and then passed the unchanged serial suite: 1,061 library tests, 86
CLI tests, and 254 integration tests, with the two explicit live/release qualification
tests ignored by the normal suite. T035 remains in development only until the exact
committed revision passes hosted CI and its lifecycle move is reconciled.
