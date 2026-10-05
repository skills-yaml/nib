# T059: Development CI Without Windows Tests

**Status:** Done

State: done
Primary Feature: release-delivery

## Problem

Windows Tests has the longest successful CI duration and the highest observed
failure rate. The user requested disabling Windows for development CI.

## Scope and Design

Skip the `windows-tests` job on pushes to `development` and pull requests whose
base is `development`. Preserve the job for pushes to `main` and pull requests
whose base is `main`, including promotion from `development`. Use the event's
target branch rather than its source branch to select the job.

Release artifact builds, update qualification, Linux/macOS CI, and runtime
behavior are outside this change. Windows release assets remain required by
the release manifest and transaction contract.

## Affected Areas

- `.github/workflows/ci.yml`: Windows job condition.
- `workspace/instructions/tech/ci.md`: development versus production validation coverage.
- `workspace/specs/README.md`: lifecycle catalog.
- `workspace/agents/memory/decisions.md`: stable development CI decision.

## Acceptance Criteria

- [x] Windows Tests skips development pushes and PRs targeting development.
- [x] Windows Tests runs on main pushes and PRs targeting main, including a
  development-to-main PR.
- [x] Linux/macOS jobs and Windows release artifacts retain their existing gates.
- [x] CI documentation describes the branch-specific Windows gate.
- [x] `task verify` and `task docs:check` pass, and the complete diff is reviewed.

## Implementation Plan

1. Add a Windows job condition selecting the main push ref or main PR base.
2. Document the branch scope and inspect push/PR cases for both target branches.
3. Run the canonical gates, review spec compliance and change quality, and
   reconcile the spec and memory against actual evidence.

## Validation Gates

- Review the condition for development/main pushes and PRs, including promotion.
- Run `task verify` and `task docs:check`.
- Inspect the final diff to confirm that only Windows ordinary CI is conditional.

## Risks and Rollout

Development no longer detects Windows regressions through ordinary CI. Main
and main-targeting PRs retain native Windows validation. The change takes effect
after the workflow change is integrated; existing workflow runs are unaffected.
Restore unconditional job execution to roll back.

## Prior Memory Notes

Record the user's stable branch-specific CI preference in project decisions.
No persistence, delegation, external credentials, or workload-state change.

## Validation Evidence (2026-09-30)

- `task verify` passed: strict static checks, the complete serial suite, and doctests.
- `task docs:check` passed all five documentation and lifecycle checks.
- Reviewed development/main push and PR branch cases; main-targeting promotion
  selects Windows regardless of the source branch.
- Spec-compliance review confirmed the requested development-only skip.
- Quality review confirmed the condition uses the target branch, preserves Linux
  and macOS jobs, and leaves the release workflows untouched.
- Local implementation is complete; the workflow change has not been committed,
  pushed, or integrated. Hosted skip behavior applies after integration.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | historical | Retrospective classification of the preserved pre-v7 outcome; no new bump or release identity is inferred. |

## Memory Impact

Status: none
Rationale: This historical outcome is preserved; migration adds no new durable decision for this spec. Existing dated memory evidence remains authoritative.
