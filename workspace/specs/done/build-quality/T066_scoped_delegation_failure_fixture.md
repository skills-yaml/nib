# T066: Scoped Delegation Failure Fixture

**Status:** Done

State: done
Primary Feature: build-quality

## Scope

Resolve confirmed fixture-state leakage found during T062 main qualification.
Superseded CI 37354295208 Windows passed 1164 library tests but failed the
owner-failure boundary fixture, then a later deadline fixture unexpectedly
received the unconsumed global owner failure. The first actual error was not
printed, so its cause is unconfirmed. Scope injection cleanup with a test-only
RAII guard, retain exact namespace and fault-path assertions, print useful
actual-error/consumption diagnostics, and use the existing test-only 30-second
preparation guard for the owner-failure boundary fixture. Production budgets
and deadline-specific fixtures remain unchanged. No paid calls.

## Acceptance Criteria

- [x] AC-1: Owned test injection clears on normal exit and panic unwinding;
  normal consumed/unconsumed and panic paths have observable counter coverage.
- [x] AC-2: Owner-failure fixture verifies clean initial state, intended injected
  error and consumption, with actual diagnostics and exact namespace assertions.
- [x] AC-3: Only its setup uses the existing test-only preparation budget;
  production deadlines and deliberately expiring fixtures remain unchanged.
- [x] AC-4: Independent exact review, affected delegation/static/docs gates,
  frozen full native verification and revised native Windows qualification pass.
- [x] AC-5: Actual development integration and verified main merge precede done;
  catalog, version rationale and memory are reconciled.

## Affected Areas

src/tools/delegation/tests.rs, src/tools/delegation/test_part_0.rs, this spec,
canonical catalog and own peer records. Runtime source and public behavior,
Task/workflow definitions, release manifests and governed instructions stay
unchanged.

## Implementation Plan

1. Record evidence and independently reviewed bounded plan before editing.
2. Add a scoped test injection guard with observable normal/unwind cleanup;
   use it in the real owner boundary fixture and assert consumption/errors.
3. Apply the existing fixture-only 30-second setup authority for this test,
   preserving exact namespace assertions and separate expiry/production budgets.
4. Review, freeze, validate native and hosted Windows, then reconcile events.

## Validation Gates

task check, task test:delegation, task docs:check, task verify,
git diff --check and full exact-source hosted Windows/native qualification.
Linux bwrap is required locally; no provider credentials or paid calls.

## Risks and Rollback

A successful fresh run does not resolve confirmed injection leakage. Cleanup
must run under unwinding and must not change production behavior. The guard
must reject a pre-armed slot; injected-path and namespace assertions remain
strict. Do not claim the first failure was a timeout without retained evidence.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | none | none | Repairs cfg(test) fixture isolation and setup/diagnostics only; no native runtime/public artifact change. |

## Memory Impact

Status: none
Rationale: This bounded test-fixture repair restores the documented failure-isolation contract and creates no new durable project decision.

## Independent Plan Review

Reviewer t062-review independently confirmed the sole arming site and later
unconsumed injection failure. Approved scoped test-only RAII cleanup,
consumption/error diagnostics, observable normal/unwind coverage and the
existing fixture setup guard. The first underlying error remains unknown;
production deadlines and expiry assertions are preserved.

## Validation Evidence

Candidate 54a97e73114c5a503b46b4c931d10333ce17beb9 passed task check and
task test:delegation with strict native bwrap enabled. The selected delegation
target does not execute the new guard cases or changed owner-boundary case;
the frozen full task test and revised hosted Windows run must execute them.
Independent exact-candidate spec and quality review found no findings.

## Frozen Native Evidence

Independent exact author/combined and projected-stage review found no findings.
Frozen stage 42550166c79df8d7acf4d0c405f65ce04d5947e8 passed fresh ordered
task check and task test with NIB_REQUIRE_BWRAP_TESTS=1 on 2026-10-05.
These commands exactly compose task verify; the internal integration ref
advanced only after both succeeded. Full native tests execute all new guard
cases and the changed owner-boundary fixture; selected delegation evidence
is not substituted. Hosted Windows and actual delivery remain pending.

## Integration Evidence

Revision: 30e9ecf8151768ca236367ed4984e1a06bc96380
Outcome: passed

PR45 and the actual remote ref confirm verified development integration
on 2026-10-05T19:51:41Z. Independently reviewed source ddff42a781ff1566ce6bc3cd4cc1a45e2f632607 and
exact merge previews passed CI 37361857527 (development Linux/macOS).
Main all-platform qualification remains required before done. Full development suites, strict native
containment, exact binary identities, coverage and native smokes passed.
Frozen native stage 42550166c79df8d7acf4d0c405f65ce04d5947e8 passed fresh ordered task check
and task test, exactly composing task verify. The new T066 guard/owner cases
executed in the full native suites; earlier selected-test evidence is not substituted.
Publication is tracked separately. Main reconciliation and final frozen lifecycle verification remain pending.

## Main Evidence

Revision: 4b0245b9890bb15a4c99ff46ec7296d42bf9401f
Outcome: passed

PR43 and the actual remote ref confirm verified main merge
on 2026-10-05T20:20:23Z. Independently reviewed source ddff42a781ff1566ce6bc3cd4cc1a45e2f632607 and
exact merge previews passed CI 37361857527 (development Linux/macOS)
and CI 37361857315 (main Linux/macOS/Windows). Full suites, strict native
containment, exact binary identities, coverage and native smokes passed.
Frozen native stage 42550166c79df8d7acf4d0c405f65ce04d5947e8 passed fresh ordered task check
and task test, exactly composing task verify. The new T066 guard/owner cases
executed in the full native suites; earlier selected-test evidence is not substituted.
Publication is tracked separately. Final event/catalog/memory records receive renewed exact review and frozen canonical verification before delivery.

All acceptance criteria have implementation/review/validation evidence above.
T062/T063/T065 memory is updated; T064/T066 memory is none because these
bounded fixture repairs restore existing contracts without new durable context.
