# T064: Interrupt Smoke Input Lifetime

**Status:** Test

State: test
Primary Feature: release-delivery

## Scope

Resolve the confirmed macOS offline qualification race discovered during
[T062](../workspace-governance/T062_workspace_docs_v7_upgrade.md) delivery.
All three Unix Ctrl+C feeders currently return immediately, closing stdin;
macOS script translates the closure to Ctrl+D and EOF can win before cancellation.
Keep stdin open until the exact expected child status 130 appears, using the
existing bounded output wait. Preserve every exit-status, Run cancelled,
persisted cancellation, forbidden-artifact and terminal-restoration assertion.
No product runtime or published interface change.

## Acceptance Criteria

- [x] AC-1: Question, approval and terminal interrupt feeders send Ctrl+C and
  wait with a bound for the exact expected child status before closing stdin.
- [x] AC-2: Existing interruption, artifact, persisted-state and terminal-mode
  assertions remain unchanged; no blind sleep or weakened acceptance.
- [ ] AC-3: Independent exact-candidate review, installer/Task/static gates,
  complete native gates and revised exact-source macOS qualification pass.
- [ ] AC-4: Confirmed development integration and main merge precede done;
  catalog, version rationale and memory are reconciled.

## Affected Areas

scripts/check-interactive-release.sh, this spec, canonical catalog and own
peer work records. Runtime code, Task commands, workflows, manifests and
instruction policies remain unchanged.

## Implementation Plan

1. Record the failed development CI 37338656478 evidence and bounded scope.
2. Keep each Ctrl+C feeder open using wait_for_pty_output on the exact child
   status marker 130, retaining the existing bound and assertions.
3. Independently review and run affected native gates; requalify the revised
   exact candidate on macOS before development/main delivery.
4. Reconcile verified lifecycle events, catalog and memory.

## Validation Gates

task check, task test:installers, task test:task-contract, task docs:check,
task verify, hosted native macOS task smoke:interactive:binary and exact
release-binary qualification, plus project-authored git diff --check.
No paid provider calls.

## Risks and Rollback

The wait must observe the child exit marker, not merely a question prompt or
terminal restoration. An unexpected status or missing marker must fail within
the existing bound. Preserve stdin ownership until cancellation completes;
do not roll back to the confirmed EOF race or relax assertions.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | none | none | Repairs offline qualification feeder timing only; changes no versioned runtime/public artifact or native binary. |

## Memory Impact

Status: none
Rationale: Restores the already documented offline feeder input lifetime; no new durable product contract or decision beyond T062/T063 delivery.

## Independent Plan Review

Reviewer t062-review confirmed the existing macOS Ctrl+D translation and the
Rust SIGINT fixture's open-stdin ownership pattern. Approved bounded feeder
repair and justified none version impact before implementation. The passing
main candidate smoke is intermittent evidence, not resolution of this race.

## Integration Evidence

Revision: 3cc4550b7d807f39a5fecf07196ee2b703503b24
Outcome: passed

PR44 and the remote development ref confirm actual integration on
2026-10-05T17:35:20Z. This is the exact independently reviewed, CI-qualified
merge preview; its tree equals approved combined source 6a645e2.
CI 37344993093 passed Linux/macOS full tests, 84.26 percent runtime line
coverage (101546/120517), exact-release qualification and native interaction
smokes, including all three repaired interruption cases. Both qualified
binaries identify the exact revision above. Independent source review approved
ff1050d, combined review approved 6a645e2 and staged peer review approved f5c38a1.
Frozen task verify passed on 9528c7ce80d68480914acea842b17a9521b38e30 with
required native bwrap: 1265 library tests, 93 CLI tests, all integration suites
and doctests. Serialized T064 peer integration f5c38a1 also passed fresh task
check and task test. Main all-platform qualification and completion remain pending.
