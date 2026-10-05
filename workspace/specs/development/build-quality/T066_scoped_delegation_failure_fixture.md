# T066: Scoped Delegation Failure Fixture

**Status:** Development

State: development
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

- [ ] AC-1: Owned test injection clears on normal exit and panic unwinding;
  normal consumed/unconsumed and panic paths have observable counter coverage.
- [ ] AC-2: Owner-failure fixture verifies clean initial state, intended injected
  error and consumption, with actual diagnostics and exact namespace assertions.
- [ ] AC-3: Only its setup uses the existing test-only preparation budget;
  production deadlines and deliberately expiring fixtures remain unchanged.
- [ ] AC-4: Independent exact review, affected delegation/static/docs gates,
  frozen full native verification and revised native Windows qualification pass.
- [ ] AC-5: Actual development integration and verified main merge precede done;
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

Status: pending
Rationale: Classify the bounded fixture repair after review and validation.

## Independent Plan Review

Reviewer t062-review independently confirmed the sole arming site and later
unconsumed injection failure. Approved scoped test-only RAII cleanup,
consumption/error diagnostics, observable normal/unwind coverage and the
existing fixture setup guard. The first underlying error remains unknown;
production deadlines and expiry assertions are preserved.
