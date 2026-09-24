# T051: Windows MCP Provider-Failure Test Stack

**Status:** Done

**Related:** [T026](../done/T026_actionable_redaction_safe_llm_failure_reporting.md),
[T046](../done/T046_cross_platform_ci_repairs.md), and
[FT-016](../done/ft_016_mcp_server_exposure.md).

## Problem

The `development` CI runs for commits `28476ca` and `564e5a7` both fail in the
Windows `task test` job. The test
`nib_run_provider_failure_reaches_mcp_status_as_typed_llm_error` aborts with
`STATUS_STACK_OVERFLOW`; Linux and macOS pass, and the Windows MCP native smoke
passes. Because the test process exits abruptly, it cannot prove the provider
failure and status boundary on Windows.

## Goals

- Identify the stack-heavy test or production call boundary responsible for the
  failure under the default Windows test-thread stack.
- Keep the same provider-failure, persisted-session, status, redaction, and bounded
  output assertions while making the test pass on native Windows.
- Keep native CI's full `task test` gate intact; do not suppress, ignore, or skip the
  failing test, or raise the stack limit for the whole suite.

## Non-Goals

- Change MCP wire schemas, provider error classification, delegation persistence,
  or release publication.
- Redesign unrelated Windows tests or tune global CI runner limits.

## Implementation Plan

1. Measure or bound the failing async/test stack on Linux at a Windows-sized
   stack and inspect large futures and values retained across suspension points.
2. Separate the proven stack-heavy work into a bounded frame or task while
   preserving the test's observable assertions and production semantics. If a
   production call boundary is responsible, fix that boundary rather than only
   increasing the test thread's stack.
3. Run the exact test and surrounding MCP tests locally, then `task check`,
   `task docs:check`, and `task verify`. Confirm the native Windows `task test`
   result on a PR before completion.

## Affected Areas

- `src/integrations/mcp_server.rs`: failing test and, only if demonstrated,
  its MCP request boundary.
- `Taskfile.yml` if a repeatable focused stack probe is needed.
- `docs/specs/README.md` and this spec for state and validation evidence.

## Acceptance Criteria

- [x] The exact test passes on native Windows with its ordinary test-thread stack,
  with every existing typed failure and redaction assertion retained.
- [x] Windows `task test` passes; Linux and macOS gates remain green.
- [x] `task check`, `task docs:check`, and `task verify` pass on the final tree.
- [x] The fix does not alter production MCP responses or introduce a global stack
  increase, ignored test, or relaxed security assertion.

## Validation Gates

- Focused exact test and MCP test group while iterating.
- `task check`, `task docs:check`, and `task verify` locally.
- Full native Windows CI `task test`, plus Linux and macOS CI, before moving to done.

## Risks and Mitigations

- A Linux-only reproduction may not match Windows stack layout. Use it to locate
  pressure, then require native Windows CI as acceptance.
- Splitting an async fixture can accidentally drop data or weaken the privacy
  check. Keep the existing assertions and compare the same persisted surfaces.

## Rollout

Merge into `development` after native CI passes. The change needs no migration.

## Open Questions

None. The scoped production dispatch future was the stack-heavy boundary.

## Implementation Findings (2026-09-24)

- The unchanged test reproduces `STATUS_STACK_OVERFLOW` on Linux when its test
  thread is limited to one MiB. The failure occurs before
  `handle_request_with_cancellation` starts polling.
- The public `handle_request` future measured about 82 KiB because it stored the
  complete dispatch future inside the session lock-policy scope. Boxing that inner
  future reduced the outer future to 144 bytes and the unchanged test then passed
  on the one MiB stack. This also reduces stack pressure in production MCP requests
  at the cost of one heap allocation per request.
- `task test:mcp-stack` compiles with the normal stack first, then runs the exact
  test with `RUST_MIN_STACK=1048576`; applying that variable during compilation
  caused `rustc` itself to overflow before the test ran.
- The first `task verify` passed the MCP test but an unrelated installer
  staged-asset-visibility fixture failed. `task test:installers` then passed all
  42 tests, including that fixture. The full `task verify` rerun passed.
- The exact MCP provider-failure test passed on both native Windows CI attempts
  for PR #30. The full Windows job then failed in the separate TUI cache test;
  [T052](T052_windows_tui_session_cache_refresh.md) owns that repair.
- The final PR run
  [36010434574](https://github.com/skills-yaml/nib/actions/runs/36010434574)
  passed Windows, macOS, and Linux validation. Its Windows log shows the exact MCP
  provider-failure test passing with the standard test-thread stack.
