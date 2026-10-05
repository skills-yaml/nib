# T063: Isolated Read-Only Git Status

**Status:** Development

State: development
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
- [ ] AC-4: Independent exact-candidate review, task test:git-status, task docs:check
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
