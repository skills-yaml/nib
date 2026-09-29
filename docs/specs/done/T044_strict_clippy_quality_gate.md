# T044: Strict Clippy Quality Gate

**Status:** Done

**Related:** [Backend Rust](../../tech/backend_rust.md),
[Task Runner](../../tech/task.md), [CI](../../tech/ci.md),
[SDLC](../../tech/sdlc.md),
[T035 Fast Incremental Check](../done/T035_fast_incremental_check_and_single_full_verification.md)

## Objective and Scope

Adopt the common strict Rust-project Clippy baseline for nib: lint every local Rust
target and feature with the default `clippy::all` policy, add the non-default
`too_many_lines` maintainability lint, and treat every emitted Rust or Clippy warning
as a build error.

This task covers repository lint policy, Task orchestration, deterministic contract
coverage, documentation, and the source refactors required to make the new gate green.
Existing narrow lint exceptions remain valid when they document an intentional API or
test tradeoff. The audited legacy function-length debt may use exact, reason-bearing
`#[expect(clippy::too_many_lines)]` attributes: new long functions still fail, and a
stale expectation fails once its function is shortened. Plain `allow` suppression and
crate-wide, module-wide, or test-wide exceptions for `too_many_lines` are forbidden.

Out of scope: enabling all `pedantic`, `nursery`, `cargo`, or `restriction` lints;
changing runtime behavior; setting a total-file-length limit; broad module
decomposition; dependency upgrades; live-provider qualification; releases.

## Acceptance Criteria

- [x] `Cargo.toml` explicitly denies the `clippy::all` group and
      `clippy::too_many_lines`, with group priority permitting narrower documented
      exceptions.
- [x] `.clippy.toml` records the standard 100-line function threshold.
- [x] `task check` runs Clippy against all targets and all features and denies every
      warning while retaining its fast static-only contract.
- [x] `task fix` applies Clippy fixes to the same target and feature surface.
- [x] Every pre-existing `too_many_lines` violation is either removed through
      behavior-preserving extraction or recorded with the exact T044 legacy
      expectation; no plain `allow` or broad-scope suppression is introduced.
- [x] New long functions fail the gate, while shortening a baselined function causes
      its stale expectation to fail under warning denial.
- [x] Static Task/config contract coverage fails if the strict lint policy, threshold,
      target coverage, feature coverage, or warning denial is weakened.
- [x] Rust, Task, and CI documentation describe the enforced policy accurately.
- [x] `task test:task-contract`, `task docs:check`, `task check:all-targets`, and
      `task verify` pass, followed by `git diff --check` and two-stage self-review.

## Affected Areas

- `Cargo.toml` and `.clippy.toml` — lint levels and the function-length threshold.
- `Taskfile.yml` — canonical lint and fix commands.
- `tests/installers.rs` — deterministic repository-policy regression coverage.
- Rust functions reported by the strict gate — behavior-preserving helper extraction.
- `docs/tech/task.md`, `docs/tech/backend_rust.md`, and `docs/tech/ci.md` — quality
  gate documentation.
- `docs/specs/README.md` and this spec — lifecycle status and validation evidence.

The workload model, persistence, delegation, approval, external-system, and user-facing
interfaces are unaffected.

## Implementation Plan

1. Add a contract test for the manifest, Clippy configuration, and Task commands.
2. Declare the strict lint policy and expand `task check`/`task fix` to all targets and
   features.
3. Run the focused contract and static gate to inventory violations.
4. Resolve small `too_many_lines` findings through helper extraction where safe and
   record the remaining audited legacy debt with exact reason-bearing expectations.
5. Update authoritative technical documentation and reconcile this spec.
6. Perform spec-compliance review, code-quality review, and all validation gates.

## Validation Gates

All repeatable validation runs through Task:

1. `task test:task-contract`
2. `task docs:check`
3. `task check`
4. `task check:all-targets`
5. Focused tests for any source module changed during extraction
6. `task verify`
7. `git diff --check` for final patch hygiene

No credentialed, paid, or network-dependent runtime qualification is required.

## Risks and Mitigations

- **Existing long functions make the new gate immediately red:** Inventory first;
  retain strict enforcement with exact `expect` attributes that fail when stale,
  rather than weakening the threshold or using silent `allow` attributes.
- **All-target linting increases fast-gate time:** Keep tests unexecuted in `task check`;
  measure the gate and preserve `task verify` as the only complete local aggregate.
- **All-feature builds expose incompatible feature combinations:** nib already defines
  `task check:all-targets` with `--all-features`; retain that established supported
  surface.
- **A Clippy upgrade introduces a new warning:** Warning denial intentionally makes the
  drift visible; resolve it or add the narrowest reviewed exception with a reason.
- **Helper extraction changes behavior:** Preserve ordering, ownership, errors, and
  side effects; run focused tests plus the complete serial suite.

## Implementation Reconciliation (2026-09-17)

The repository now declares `clippy::all` and `clippy::too_many_lines` as denied lint
levels. The lower-priority group declaration leaves room for narrow reviewed lint
decisions, while `task check` adds command-line warning denial and compiles every local
target and feature. `task fix` uses the same target and feature surface. The checked-in
Clippy configuration fixes the function threshold at 100 lines.

The first complete lint inventory found 184 pre-existing long functions across 47 Rust
files. Each is recorded with the exact reason-bearing T044 expectation. The repository
contract rejects plain `allow` suppression and any differently formed
`too_many_lines` expectation, including whitespace and multiline variants. Because
unfulfilled expectations are warnings and the Task gate denies warnings, shortening a
baselined function also fails until its obsolete marker is removed. New unmarked long
functions fail directly.

All additional `clippy::all` findings exposed by the wider target surface were resolved
with behavior-preserving simplifications. The only separate lint expectation is the
narrow reason-bearing `too_many_arguments` decision for the live qualification report
constructor. No runtime interface, workload-state transition, or external integration
contract changed.

## Validation Evidence (2026-09-17)

- `task test:task-contract` passed both repository-policy contract tests.
- `task docs:check`, `task check`, and `task check:all-targets` passed.
- `task test:delegation`, `task test:updater`, `task test:interactive`, and
  `task test:llm-live:offline` passed.
- Final `task verify` passed strict formatting and all-target/all-feature Clippy, 1,157
  library tests, 86 binary tests, every deterministic integration target, and doctests.
  The credentialed live-provider test and optimized release qualification remained
  explicitly ignored as designed.
- Spec-compliance review found every acceptance criterion implemented. Code-quality
  review strengthened the policy scanner against multiline suppressions and found no
  unintended runtime behavior changes. `git diff --check` passed.
