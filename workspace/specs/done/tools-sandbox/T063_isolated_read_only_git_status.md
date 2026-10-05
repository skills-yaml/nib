# T063: Isolated Read-Only Git Status

**Status:** Done

State: done
Primary Feature: tools-sandbox

## Scope

Resolve the independent main-promotion review finding on T058's git_status
tool before T062 is delivered. Git status may execute repository fsmonitor
hooks and clean/process filters, including in populated submodules. Reuse
the existing strict bwrap boundary with the repository read-only, no network,
masked private home and allowlisted child environment. Disable fsmonitor and
optional locks, preserve repository filter/submodule semantics inside isolation,
and fail closed with an actionable error when strict isolation is unavailable.
This is a linked follow-up; T058's historical done record is preserved.

Related: [T058](../../done/tools-sandbox/T058_preflight_diagnostics_and_independent_read_progress.md),
[T062](../workspace-governance/T062_workspace_docs_v7_upgrade.md).

## Acceptance Criteria

- [x] AC-1: git_status cannot execute unisolated helpers, create a session worktree,
  write the original repository/index, access ambient credentials/private home,
  or use the host network. All writable cwd bindings are replaced with exact
  read-only bindings; unavailable isolation returns an error without a fallback.
- [x] AC-2: Ordinary status still reports changed/untracked files where strict
  isolation is usable, within the existing 15-second and 65536-byte bounds.
  Managed child cleanup covers helper descendants on exit, cancellation and timeout.
- [x] AC-3: Observable fixtures cover fsmonitor, executable filters, populated
  submodules, private environment/home, index preservation and unavailable isolation.
- [x] AC-4: Independent exact-candidate review, task test:git-status, task docs:check
  and task verify pass; guide, catalog, version and memory records are reconciled.

## Affected Areas

src/tools/core.rs, src/tools/core_tests.rs, src/tools/git_status_tests.rs,
src/sandbox/mod.rs, Taskfile.yml,
workspace/docs/user/guide.md, spec catalog, release ledger and project memory.

## Implementation Plan

1. Record the review defect and bounded isolation contract before implementation.
2. Build a strict read-only command from existing bwrap arguments with exact
   cwd-binding validation; retain home masking and environment filtering.
3. Spawn through existing ManagedChild cleanup and keep output/deadline bounds.
4. Add adversarial observable fixtures and unavailable-platform coverage.
5. Review, freeze, verify and deliver with T062 through development and main.

## Validation Gates

task test:git-status, task docs:check, task verify and project-authored
git diff --check. Hosted Linux exercises strict native fixtures when bwrap is
usable; other platforms verify the fail-closed result. No live provider calls.

## Risks and Rollback

This tool is bounded to isolated repository configuration: private HOME and
global/system Git configuration are not inherited. Repositories depending on
global-only filters/autocrlf may need an approved terminal inspection. Existing
linked worktrees whose Git metadata is masked may return a bounded Git failure
rather than expose additional private home paths. Windows, macOS and Linux
without usable strict bwrap return actionable errors; they never report a false
empty status. Rollback may disable this tool, but may not restore unisolated
execution under its ReadOnly classification.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-next | Closes an unapproved executable-helper path in the same applied integration release. |

## Memory Impact

Status: updated
Rationale: The accepted strict read-only Git-status boundary and isolated-configuration
limit are recorded in workspace/agents/memory/decisions.md and
workspace/agents/memory/changelog.md. Delivery evidence remains separate.

## Independent Finding and Plan Review

Reviewer t062-review identified host execution of core.fsmonitor and clean/process
filters in the combined main candidate 6a2e5934fc817bed832f613f16d9e04cc050aca6.
The reviewer approved the strict read-only/no-network implementation direction
before edits, including unavailable errors and the isolated-configuration limit.
The superseded final verification was interrupted before source changes; it is
not completion evidence for this follow-up.


## Focused Implementation Evidence

The strict constructor rejects unavailable platforms/capabilities before spawning.
Its exact cwd binding is read-only, network isolation is mandatory, fsmonitor is
disabled, optional locks are disabled, and ambient Git configuration is cleared.
The existing ManagedChild process-tree cleanup owns the bwrap process and retains
the 15-second timeout and 65536-byte output bound. Private-home-root repositories
are rejected rather than leave the home exposed.

All eight task test:git-status cases passed with NIB_REQUIRE_BWRAP_TESTS=1 on a
usable native Linux bwrap backend. Executable clean/process filters and a populated
submodule could not write sentinels or change either index. Synthetic private
home/environment values were inaccessible; the process-filter fixture could not
connect to a listening host loopback socket. Its committed-content response proves
Git actually executed and accepted that filter inside isolation. Unavailable-route
and mount-layout fixtures run on all platforms. Hosted Linux requires the strict
backend; unavailable native backends may not silently skip those fixtures there.

Independent exact-candidate review and fresh complete verification are pending.

## Review Repair

Review of e7eca590 found missing/invalid HOME could leave private home unmasked.
The repaired constructor requires a canonical HOME directory, verifies its exact
tmpfs mask, and pins that same canonical directory on the child environment.
Missing, nonexistent, non-directory and unmaskable homes fail before spawning.
Deterministic regression cases also prove the child cannot inherit a later HOME
change. Renewed exact-candidate review and final verification remain required.

## Integration Evidence

Revision: 8bc243d00ec3ec82e7287c704fcdd897ff78204a
Outcome: passed

Confirmed shared development integration on 2026-10-05 through PR42. Independent
review approved exact source 7b5b8e506daa2c61377c4632918d1ceed14b5bc1 and the
exact CI-qualified merge revision above; their complete trees are identical.
Frozen task verify passed with required native bwrap: all 1265 library tests,
93 CLI tests, every integration suite and doctests. Serialized peer integration
ac8083b768315de65f764dc80a5e3af5170b8471 also passed every registered native gate.
Hosted CI37333585002 passed Linux/macOS tests, 84.26% runtime line coverage,
exact-source release-binary qualification, native interaction and managed-process
smokes. Both qualified binaries identify this exact development revision.

The remote development ref and PR42 merged state confirm the actual event.
Main promotion and Windows qualification remain required; publication is separate.
The repaired ten-case native Git-status suite passed before the frozen full run,
and all independent findings are resolved. Historical done records are unchanged.

## Main Merge Evidence

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
