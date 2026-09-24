# T052: Windows TUI Session Cache Refresh

**Status:** Done

**Related:** [T048](../done/T048_active_tui_render_responsiveness.md) and
[T051](T051_windows_mcp_provider_failure_stack.md).

## Problem

The Windows `task test` job failed twice on the same commit in
`session_display_cache_refreshes_on_cadence_and_session_switch`. Its five-second
settle bound expired, while the MCP provider-failure test passed on both attempts.
The TUI background reader gives the session store only 10 ms to acquire and check
its file lock. Native Windows file operations under CI load can consume that budget
before the read starts, so every refresh attempt can be discarded.

## Scope and Goals

- Allow a background TUI session read enough time for normal Windows file-lock and
  directory work while keeping the UI frame path nonblocking.
- Preserve the one-second refresh cadence, last-valid-snapshot behavior, and
  immediate cache reset on session switch.
- Make the existing Windows cache test pass without skipping it or relaxing its
  five-second settle bound.

## Non-Goals

- Change session persistence, locking authority, or the TUI rendering architecture.
- Change the MCP request fix tracked by T051.

## Implementation Plan

1. Increase only the background session read's lock deadline from 10 ms to 250 ms.
   The worker remains detached from the frame loop and attempts at most one read
   per second.
2. Add a focused test that holds the session lock briefly and proves a single
   refresh attempt can return the session after the lock is released. Keep the
   existing cadence and switch test and its five-second settle bound unchanged.
3. Run focused TUI tests, `task check`, `task docs:check`, and `task verify` locally.
   Require native Windows `task test` and the other CI jobs to pass before done.

## Affected Areas

- `src/tui/mod.rs`: background session read deadline and regression test.
- `Taskfile.yml`: focused cache regression task.
- `docs/specs/README.md` and this spec: lifecycle and validation evidence.

## Acceptance Criteria

- [x] A single background refresh succeeds after a brief session-lock hold.
- [x] The existing cadence and session-switch test passes unchanged on native
  Windows, including its five-second settle bound.
- [x] The TUI frame path stays nonblocking and refresh attempts remain limited to
  one per second.
- [x] `task check`, `task docs:check`, `task verify`, and native Windows `task test`
  pass; Linux and macOS CI remain green.

## Validation Gates

- `task test:tui-cache` and `task test:interactive` during implementation.
- `task check`, `task docs:check`, and complete `task verify` locally.
- Full native Windows, macOS, and Linux CI before moving to done.

## Risks and Alternatives

A longer background read can retain a worker longer under lock contention. The
frame loop only polls a channel and stays responsive; the next attempt still waits
for the existing one-second cadence. Extending only the test's five-second bound
would hide stale snapshots and leave the production 10 ms deadline unchanged.

## Rollout

Merge with the Windows MCP stack fix after native CI passes. No migration is needed.

## Open Questions

None. Native Windows CI validated the 250 ms deadline under the project runner's
filesystem load.

## Validation Record (2026-09-24)

The pre-fix native Windows CI attempts both failed in the unchanged cache cadence
test after five seconds; each passed the MCP provider-failure test. On the repair
branch, `task test:tui-cache`, `task test:interactive`, `task docs:check`, and full
`task verify` passed locally. `git diff --check` passed. The final PR run
[36010434574](https://github.com/skills-yaml/nib/actions/runs/36010434574)
passed Windows, macOS, and Linux validation. Its Windows log shows both the
unchanged cadence/session-switch test and the new held-lock test passing.
