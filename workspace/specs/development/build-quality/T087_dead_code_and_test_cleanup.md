# T087: Dead Code and Test Cleanup

**Status:** Development. Started on 2026-10-22 under a user-requested goal. It
replaces an uncommitted draft from another session (numbered T086 there, which
collided with the merged startup-screen spec); this spec is rewritten from the
draft's recorded findings.
State: development
Primary Feature: build-quality

## Problem and Evidence

Reviews of revision `82aadb7` found:

- unused exported Rust items in the library;
- tests that maintain an obsolete context and skill-selection path;
- three focused Task commands (`task test:tui-cache`, `task test:tui-shutdown`
  and `task test:mcp-stack`) that exit successfully with zero tests run,
  because the tests moved into submodules while the Task filters kept their
  old paths. The full suite still runs those tests, but not the MCP
  regression's dedicated one MiB stack execution.

Separately, Linux CI's interactive release smoke has repeatedly been killed by
the job timeout (exit 137) in the editor cases (`tui-editor-failure` twice,
`tui-editor-success` once) on unrelated pull requests; reruns passed.

## Scope

1. Point each focused Task filter at the tests' current paths, and make every
   focused test task fail when it runs zero tests.
2. Remove exported items that have no consumer in the crate, its binaries,
   tests or examples, unless they are documented public library API; remove
   or update tests that only keep the obsolete context/skill path alive.
3. Make the editor smoke cases wait for an observable signal instead of fixed
   delays, so a slow runner cannot hang them into the job timeout.

Out of scope: behavior changes, new features and refactors beyond removal.

## Affected Areas

`Taskfile.yml` (focused test tasks), library modules with unused exports and
their tests, `scripts/check-interactive-release.sh` (editor cases), and the
Task contract test if it checks filters.

## Acceptance Criteria

- [ ] AC-1: `task test:tui-cache`, `task test:tui-shutdown` and
  `task test:mcp-stack` each run at least one test, and the MCP regression
  runs on its one MiB stack; a focused task that matches zero tests fails.
- [ ] AC-2: Every removed item had no consumer; `cargo check --all-targets`,
  clippy with `-D warnings`, and the full test suite pass with no behavior
  change. Removed tests are listed with the reason.
- [ ] AC-3: The editor smoke cases wait on observable output with bounded
  timeouts and pass repeatedly in local runs.
- [ ] AC-4: `task verify` with `NIB_REQUIRE_BWRAP_TESTS=1` and the interactive
  release smoke pass.

## Validation Gates

Focused Task commands while iterating; `task check`, `task test:task-contract`
and frozen `task verify` before handoff.

## Risks, Rollback and Rollout Plan

Removing an item a downstream library user relies on: keep documented public
API, and remove only items with no consumer and no documentation. Each removal
reverts independently.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-goal-minor | Cleanup with no behavior change; joins the goal minor 0.6.0 from published 0.5.2. |

## Memory Impact

Status: pending
Rationale: Classified when the cleanup is verified; a durable fact (focused
test tasks must fail on zero tests) may result.
