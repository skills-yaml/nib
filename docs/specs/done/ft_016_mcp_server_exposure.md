# FT-016: MCP Server Exposing the Agent Loop

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Build an MCP server embedded in `nib` that exposes the `nib` agent loop itself as a tool, allowing other tools or IDES (like Claude Desktop) to delegate workloads to the `nib` CLI safely.

## Problem Statement
At the feature baseline, `nib` consumed MCP tools as a client but could not be
orchestrated by other MCP systems. The reconciliation below records the shipped stdio
server boundary.

## Goals
- Implement an MCP server endpoint in `src/integrations/mcp_server.rs`.
- Expose tools like `nib_run(goal)` and `nib_get_status(session_id)`.
- Allow external systems to leverage `nib`'s gated execution model, hybrid sandboxing, and session persistence natively.

## Scope
- Create `src/integrations/mcp_server.rs` module.
- Implement an MCP server that responds over stdio.
- Add tool `nib_run` to start a background `nib` task.
- Add tool `nib_get_status` to query the status of an agent run using its session_id.
- Add a new CLI command to start the MCP server (e.g. `nib mcp-server`).

## Acceptance Criteria
- `nib mcp-server` starts an MCP stdio server.
- The server advertises `nib_run` and `nib_get_status` tools.
- `nib_run` starts a task via `spawn_subagent` logic or directly spawning `run_agent_loop`.
- `nib_get_status` retrieves the status of the background task.
- Code passes `task check`.
- Tests verify the JSON-RPC interface for the server.

## Affected Areas
- `src/integrations/mcp_server.rs` (new)
- `src/integrations/mod.rs`
- `src/main.rs` (new CLI command)

## Validation Gates
- `task check`
- `task test`

## Reopened Audit (2026-07-15)

Scope: route inbound calls through gated/audited execution, return status for the
requested session, expose core tools safely, and test JSON-RPC behavior.

Affected areas: `src/integrations/mcp_server.rs`, `src/tools/`, task/session status,
CLI startup, and MCP server integration tests.

Validation gates: initialize/list/call/error/status/no-bypass tests, `task check`,
and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Serve bounded JSON-RPC/MCP over stdio, advertise `nib_run`, status aliases, and gated
core tools, and route calls through profile-aware runtime/executor ownership.

### Acceptance Criteria

- [x] `nib mcp-server` runs the stdio server and supports MCP initialize/list/call.
- [x] `nib_run` starts the regular agent loop and status resolves only the requested session.
- [x] Advertised core tools use registry schemas and normal executor approval/audit.
- [x] Risky noninteractive calls fail closed without creating bypass audit state.
- [x] Unknown tools, invalid arguments, notifications, and oversized output return bounded protocol responses.
- [x] Fresh local aggregate gates and Linux release-binary smoke are green.
- [x] Windows runtime gates are green.

### Affected Areas

`src/integrations/mcp_server.rs`, `src/integrations/mcp_framing.rs`, `src/main.rs`,
`src/tools/`, profile sessions, and MCP server tests.

### Implementation Evidence

`run_mcp_server`, `handle_request`, `advertised_tools`, and `call_tool` in
`src/integrations/mcp_server.rs` implement the stdio protocol and gated dispatch.

### Validation Evidence

Named MCP server tests cover list schemas, bounded responses, audited read, selected
profile/environment, destructive denial, exact status, protocol errors, and invalid-call no-audit.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
cancellation, lifecycle, worktree, and platform gates are authoritative for completion.

- [x] Initialize/list/call/status/error/no-bypass tests exist.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

Inbound HTTP/SSE and OAuth are not implemented. The shipped and documented v1 server
is stdio-only. The cancellation and lifecycle remediations below supersede this earlier
assessment of the remaining in-scope work.

## Final Cancellation Review Remediation (2026-07-15)

### Scope

Keep reading bounded stdio frames while tool requests execute, track active requests by
JSON-RPC ID, and make MCP cancellation or transport loss stop the corresponding gated
execution promptly while retaining normal audit/reconciliation behavior.

### Acceptance Criteria

- [x] `notifications/cancelled` targets only the named active request and interrupts its
  in-flight tool/agent execution without sending a notification response.
- [x] Stdin EOF or fatal transport failure cancels and joins every active request before
  the MCP server exits; group-contained terminal descendants do not continue after
  disconnect.
- [x] Concurrent request completion and cancellation produce at most one bounded response
  for each request ID, while unrelated requests remain independent.
- [x] Stdout backpressure cannot block the coordinator from consuming EOF or fatal input;
  response buffering is bounded and a blocked writer is cancelled during shutdown.
- [x] Cancellation of `nib_run` is reconciled against its detached subagent commit: a
  cancelled response cannot coexist with a continuing untracked agent, while a completed
  commit may instead win and return its normal authoritative result.
- [x] Deterministic stdio/process regressions prove cancellation and disconnect terminate
  a long-running command and preserve gated audit evidence.
- [x] Windows terminal and agent children are contained in a kill-on-close Job Object
  before execution, and the configured Windows CI job proves descendant cleanup.

### Affected Areas

`src/integrations/mcp_server.rs`, `src/sandbox/mod.rs`, MCP framing/server lifecycle
helpers, executor/tool cancellation plumbing and target-specific dependency metadata
if required, and MCP server/process tests.

### Validation Gates

Focused MCP notification, cancellation, committed-subagent reconciliation, blocked-
stdout disconnect, cross-platform descendant-cleanup, and audit tests; `task test`,
`task check`, `task coverage`, Windows CI `task test`, and isolated release binary
smoke.

## Exact-Ownership Lifecycle and Worktree Remediation (2026-07-15)

### Scope

Close the remaining cancellation linearization and MCP-exposed worktree safety gaps.
Cancellation must reconcile the persisted subagent record with authoritative task state,
must not hold the request-lifecycle mutex while record/task I/O runs, and must publish one
guarded outcome. Git worktrees used by gated MCP tools must use the shared bounded,
hook-disabled, managed process runner and compensate every partial branch, registration,
or path. Synchronous process runners must provide the same process-group or Job Object
containment on Windows as asynchronous execution.

### Acceptance Criteria

- [x] `nib_run` cancellation distinguishes terminal natural completion, definitely
  cancelled task state, a still-running tracked task, and unknown/untracked state; an
  error returned after the task was cancelled can never be treated as normal completion.
- [x] A cancelled task whose record update failed is repaired under the record lock when
  possible; otherwise one cancellation audit explicitly records the persistence failure.
- [x] Before emitting a cancelled protocol response, the coordinator explicitly finalizes
  the cancellation audit and surfaces persistence failure; `Drop` is fallback-only and an
  injected event-write failure proves exactly-once retry without silently losing the marker.
- [x] Only an authoritative terminal reread returns the started result with no
  cancellation audit; running, unreadable, contradictory, or untracked reconciliation
  fails closed as an internal error rather than normal completion or `-32800`.
- [x] Request lifecycle code claims reconciliation under its mutex, releases the mutex
  before task/record cancellation I/O, and publishes only if the same generation and
  claim remain current.
- [x] Cancellation audit persistence uses one absolute deadline across in-process and
  anchored cross-process session locks plus its authoritative reread. A stuck live lock
  holder produces a surfaced internal/shutdown failure after every request process has
  been stopped, rather than blocking the MCP server indefinitely.
- [x] Session and subagent worktree creation uses bounded, hook-disabled managed Git
  execution with restricted environment handling. Compensation removes exact-owned path,
  registration, and branch state and preserves/reports an unproven registration.
- [x] Synchronous worktree creation handles non-zero `worktree add` results and every
  path/repository validation failure after Git may have created state without deleting
  artifacts for which no exact creation receipt exists.
- [x] Managed-worktree completion inherits FT-015's exact registration-creation and
  branch-lineage provenance plus cross-process receipt recovery.
- [x] Managed-worktree completion inherits FT-015's supported Unix exact-namespace
  detachment, fail-closed residual cleanup reporting, and durable receipt recovery;
  Windows runtime execution remains covered by the platform criterion below.
- [x] Windows synchronous process execution assigns the child to a kill-on-close Job
  Object before it can run, and descendant timeout/drop regressions cover that path.
- [x] Deterministic local tests cover cancelled-record write failure after task abort,
  terminal, still-running, contradictory, and unknown reconciliation, stale publication,
  partial Git failure, exact-owned compensation, and unproven-registration preservation.

### Affected Areas

`src/integrations/mcp_server.rs`, `src/integrations/worktree.rs`,
`src/sandbox/worktree.rs`, `src/sandbox/mod.rs`, `src/sandbox/windows_job.rs`,
`src/tools/delegation.rs`, task state inspection, and focused MCP/worktree tests.

### Validation Gates

Focused lifecycle arbitration, cancellation audit, worktree compensation, process-group,
and Windows compile/tests; `cargo fmt --all -- --check`, strict all-target Clippy,
`git diff --check`, and canonical Task gates before completion.

## Local Validation Evidence (2026-07-16)

The cancellation suite covers post-commit completion/cancellation arbitration,
contradictory manager/record state, unsafe manager cancellation, stale generation/CAS
publication, exactly-once audit repair, atomic explicit-versus-fallback ownership, and
lock release before reconciliation I/O. The 17-test MCP process suite covers targeted
cancellation, EOF/fatal-input shutdown,
bounded response ownership/backpressure, direct-root-exit inherited-stdio cleanup,
direct-child reaping, group-contained descendant termination, partial Git failure, and
preservation of unproven registrations. Its single-worker lock regressions cover audit
session initialization, attempted audit, and final audit persistence.

The reconciled Linux tree passed `task check`, `task test` (772 top-level tests),
`task coverage` at 83.94 percent (53,734/64,015), `task docs:check`, `task build`, strict
all-target check/Clippy, and isolated release-binary stdio MCP smoke. Windows runtime
containment and the inherited FT-015 ownership gates remain open.

The generic cancellation path claims audit ownership, aborts and joins execution before
waiting on durable audit persistence, performs that blocking persistence outside the
async worker pool, and publishes through the next request generation. The Linux
regression holds the session lock while proving both coordinator ping responsiveness and
terminal process-group termination, including with `TOKIO_WORKER_THREADS=1`.
EOF and fatal-input shutdown signal all execution requests first, then drive cancellation
and audit reconciliation concurrently so one blocked durable audit cannot delay stopping
another request's process group. The two-request EOF regression retains the cross-process
session lock through the complete audit deadline, proves both process groups stop, and
requires the server to exit nonzero within the bound. A separate SessionStore regression
proves the in-process deadline expires without running or persisting the queued mutation.
An additional 32-iteration barrier race proves explicit cancellation and the Drop fallback
cannot both claim audit ownership; dropping an armed guard after an explicit claim retains
the explicit event details.

## Hosted Windows Terminal-Tree Follow-up (2026-07-21)

### Scope

Make the MCP terminal-tree fixture observe a real descendant at a test-owned heartbeat
path without transporting a native Windows executable through Git-for-Windows' POSIX
shell. Present the four process-lifecycle repositories as valid Git-file worktrees so
these tests do not spend their fixed startup deadline creating an unrelated nested session
worktree before terminal dispatch. On Windows, publish an absolute fail-fast shell-entry
marker, launch a shell-native background descendant that appends to an absolute heartbeat,
and retain the parent shell in `wait`. Job assignment and inheritance then retain both the
shell parent and its MSYS descendant while avoiding the non-contract, large-debug-PE launch
that obscured the cleanup behavior under test. Keep the Unix copied native fixture, relative
heartbeat, `exec` path, and verified native trace unchanged. Do not change production shell
selection, timeout, sandbox dispatch, or Job Object ownership.

The same hosted run exposed a separate session-enumeration race while an MCP audit was
being atomically replaced. T003 owns the production fix: public strict enumeration must
join the existing skill-usage mutation lock domain without weakening corruption or
detachment failures.

### Acceptance Criteria

- [x] Windows terminal-tree commands publish an absolute fail-fast shell-entry marker,
  launch a shell-native background descendant that appends to an absolute test-owned
  heartbeat, and retain the parent in `wait` without increasing the startup timeout.
- [x] Windows lifecycle cleanup does not depend on copying, hard-linking, or launching the
  large native debug fixture through Git Bash; Unix retains its copied native fixture,
  relative heartbeat, `exec` launch, and trace verification.
- [x] The four portable process-lifecycle repositories use a valid Git-file worktree,
  poll their fixture-root heartbeat, and do not create a nested session worktree.
- [x] Public session enumeration is serialized with audit mutation while retaining
  strict persistence errors and the curator's non-recursive locked enumeration path.
- [x] Targeted cancellation, stdin disconnect, fatal input, and stdout-backpressure
  regressions start the shell descendant, stop its heartbeat, and retain cancellation
  audit evidence on Windows.
- [x] Fixture startup failures identify the exact requested heartbeat path alongside the
  bounded session audit rather than reporting only an ambiguous publication timeout.
- [x] The Unix fixture-image-relative native lifecycle trace records fixture entry, child
  entry, first flush, `current_exe()`, `current_dir()`, and raw and resolved heartbeat
  paths; success verifies those records without making trace I/O lifecycle-fatal.
- [x] The exact PR revision passes the hosted Windows job and full CI matrix.

### Affected Areas

`tests/mcp_integration.rs`, `src/mcp_test_fixture.rs`, `src/session/mod.rs`, the T003
session-persistence follow-up, FT-016 hosted Windows evidence, and the native CI matrix.

### Validation Gates

The four portable MCP terminal-descendant regressions, `task test`, `task check`,
`task docs:check`, Windows-target `task check:all-targets`, and the exact-revision hosted
CI matrix.

### Reproduction Evidence

Hosted run `29800752453` passed the repaired native Windows supervisor regression and
all outbound MCP lifecycle tests. Four later inbound MCP cancellation tests timed out
while looking only below `.nib/worktrees/sessions/*` for a relative heartbeat; their
session audit showed the admitted terminal call but no terminal result. The fixture's
`process_tree` mode remains active only after its child has observed a nonempty heartbeat,
and the Windows lifecycle fixtures that use absolute heartbeat paths passed. This makes
the cross-runtime relative-path observation the narrow fixture boundary to remove; the
run does not show a production Job cleanup failure because none of the four tests reached
its cancellation assertion.

Hosted run `29802465012` passed Linux and macOS completely, including their release
builds and smoke tests. Windows again passed the native supervisor regression and all
outbound MCP lifecycle tests, but the same four inbound tests timed out while polling
their now-absolute requested paths. The native parent cannot remain active until it has
observed a marker, so this outcome is consistent with another path-boundary mismatch but
does not expose the value received by the fixture. Current Git-for-Windows path-conversion
heuristics do not make an exact rewrite provable from this log. Passing only a basename
and resolving it inside the copied native fixture removes both shell conversion and
native-current-directory behavior from the assertion without changing production cleanup.

Hosted run `29804410327` passed Linux and macOS completely and again passed the repaired
native Windows supervisor tests. The basename-resolving fixture still did not publish a
heartbeat in four inbound tests. Each audit attempt was durable in under one second, but
the terminal result remained absent when the fixed ten-second deadline expired. The
gated executor records that attempt before it provisions a required session worktree;
that provisioning launches roughly three dozen managed Git processes on Windows before
terminal dispatch. The run therefore supersedes the earlier path-only diagnosis: the
process-lifecycle deadline includes unrelated worktree preparation. A fifth Windows test
observed `SessionStore::list_result` enumerate an audit JSON just as atomic publication
evacuated the prior generation, producing a transient `NotFound` that strict enumeration
misclassified as corruption.

Hosted run `29808737976` proved the enumeration repair: the Windows
`stdout_backpressure_does_not_block_eof_cleanup_on_windows` regression passed. Linux and
macOS also remained green. The four inbound lifecycle tests still timed out, but their
audit attempts were durable in under 100 milliseconds, consistent with the Git-file
fixture removing worktree provisioning from the startup window. Their common remaining
test-owned launch boundaries were a fresh temporary copy of the roughly 254 MB debug PE
and the absolute path passed to Git-for-Windows `exec`. The next revision replaces the
copy with a same-volume hard link and invokes that link relatively without `exec`, while
retaining the production shell and Job Object containment path.

Hosted run `29812038803` passed Linux and macOS completely and again passed the native
Windows Job supervisor and strict enumeration regressions. Replacing the executable copy
with a same-volume hard link did not publish the four requested fixture-root heartbeats;
each attempt was durable in roughly 36 milliseconds and remained active without a terminal
result for the full ten-second poll. Because `process_tree` loops only after its child has
published a heartbeat, this is consistent with `current_exe()` selecting a different peer
hard-link name and resolving the marker outside the temporary project, although the hosted
log cannot prove that path. The next revision removes both executable aliases and
`current_exe()`-relative test state, while adding a native trace that exposes every path if
startup still fails.

Hosted run `29816279256` passed Linux and macOS completely and again passed the native
Windows Job supervisor, all outbound lifecycle tests, and strict session enumeration. The
four inbound lifecycle commands still remained active for their ten-second startup window,
but neither the fixture-root heartbeat nor the precreated fixture-image trace changed. The
same original image starts directly in the passing outbound tests, while copy, hard-link,
and original-image variants all fail only behind Git Bash. This localizes the remaining
boundary to the shell's native launch before observable fixture entry. Because the command
still ended with the PE, the evidence is consistent with Bash selecting an implicit
tail-overlay path despite removal of explicit `exec`; the empty trace does not prove that
selection directly. The next revision publishes shell entry and forces a status-preserving
continuation after the fixture.

Hosted run `29819713505` passed Linux and macOS completely and again passed the native
Windows Job supervisor, direct Git Bash sandbox tests, outbound lifecycle tests, and strict
session enumeration. All four inbound lifecycle requests still remained at the durable
`tool_attempted` event for their ten-second startup window. Neither the project-root
shell-entry marker nor the fixture-image trace changed, so the run did not validate the
implicit tail-overlay hypothesis. Because the hosted unit suite already proves Git Bash
execution under the same direct Job path, the next revision removes the unrelated
Git-Bash-to-large-debug-PE hop from these cleanup regressions. Windows instead uses an
absolute fail-fast entry marker and a shell-native background descendant; Unix retains the
native fixture and trace.

Hosted run `29822780187` passed macOS completely. Its Windows job again passed the native
Job supervisor, direct shell sandbox, outbound lifecycle, and strict enumeration tests,
but the same four inbound lifecycle requests remained at `tool_attempted`. Neither the
absolute fail-fast shell-entry marker nor a heartbeat was created, even though the Windows
command now contains only shell builtins. This rules out native fixture launch, path
conversion, and tail-overlay behavior: the request is stalling between audited admission
and execution of the shell command body. The Validate job separately hit the previously
nonreproducing `detached_terminal_redacts_profile_and_config_secrets_before_persistence`
session-role race; that test passed twice locally and remains a separate flake unless a
later hosted run reproduces it.

## Hosted Pre-Shell Stage Trace (2026-07-21)

### Scope

Add a debug-build-only, explicitly environment-gated stage trace to the inbound MCP
terminal startup path. Record bounded stage names from audited executor admission through
approval, worktree resolution, sandbox capability probing, shell resolution, child spawn,
Windows Job assignment, and primary-thread resume. The portable lifecycle fixture owns the
trace path and includes its contents in startup-timeout diagnostics. Do not record command
text, arguments, environment values, or other user data, and do not change release behavior,
execution policy, timeouts, shell selection, or containment semantics.

### Acceptance Criteria

- [x] The internal stage trace is absent unless a debug build receives the dedicated test
  environment variable, and trace write failures do not affect execution.
- [x] Trace records contain stage names only and distinguish capability probing, shell
  resolution, process creation, Job assignment, and primary-thread resume.
- [x] All four portable lifecycle startup failures include the test-owned trace in their
  diagnostic output.
- [x] The exact hosted Windows run identifies the final completed startup stage before the
  shell-entry marker.

### Affected Areas

`src/tools/executor.rs`, `src/tools/core.rs`, `src/sandbox/mod.rs`,
`src/sandbox/windows_job.rs`, `tests/mcp_integration.rs`, and FT-016 validation evidence.

### Validation Gates

The four portable lifecycle tests, `task test`, `task check`, `task docs:check`, Windows
target `task check:all-targets`, `git diff --check`, and exact-revision hosted CI.

### Local Validation Evidence

The absolute-path revision passed `task fix`, `task test`, `task check`,
`task docs:check`, and
`task check:all-targets TARGET=x86_64-pc-windows-msvc`. The 25-test Linux MCP process
suite passed with all four portable terminal-descendant regressions retaining their
relative worktree heartbeat. Native Windows execution and the exact hosted CI matrix
remain open until the pushed revision completes on GitHub.

The basename-resolution revision passed `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. Its 25-test Linux MCP suite again passed all four portable lifecycle
regressions, while the Windows-target build compiled the native `current_exe()` branch.
Native Windows behavior and the exact hosted matrix remain open.

The Git-file and enumeration-serialization revision passed `task fix`, `task test`,
`task check`, `task docs:check`,
`task check:all-targets TARGET=x86_64-pc-windows-msvc`, and `git diff --check`. The
25-test Linux MCP suite passed all four Git-file lifecycle regressions without creating
a nested session worktree, and the deterministic session regression proved that strict
enumeration joins the mutation lock domain. Native Windows behavior and the exact hosted
matrix remain open.

The same-volume hard-link revision passed `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. The full 25-test Linux MCP suite again passed all four portable
lifecycle regressions, and the Windows-target build compiled the same-volume temporary
project, hard-link, relative launch, and no-`exec` branches. Native Windows execution and
the exact hosted matrix remain open.

The direct-image and project-working-directory revision passed `task fix`, `task test`,
`task check`, `task docs:check`,
`task check:all-targets TARGET=x86_64-pc-windows-msvc`, and `git diff --check`. The full
25-test Linux MCP suite passed all four portable lifecycle regressions and parsed each
fixture-entry, child-entry, and first-flush trace. The Windows target compiled the
original-image launch, `current_dir()` heartbeat resolution, and fixture-image-relative
trace branches. Native Windows execution and the exact hosted matrix remain open.

The forced-shell-continuation revision passed `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. The full 25-test Linux MCP suite again passed all four portable
lifecycle regressions and preserved the Unix copied-fixture `exec` path. The Windows target
compiled the shell-entry marker, original-image launch, and status-preserving continuation
that prevents the native fixture from remaining Bash's final external command. Native
Windows execution and the exact hosted matrix remain open.

The shell-native Windows descendant revision passed `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. The full 25-test Linux MCP suite again passed all four portable
lifecycle regressions with the Unix native fixture and trace unchanged. The Windows target
compiled the absolute fail-fast entry marker, background shell heartbeat, parent `wait`,
and Windows-only omission of native trace setup. Native Windows execution and the exact
hosted matrix remain open.

The pre-shell stage-trace revision passed `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. All four portable lifecycle tests validated the ordered executor,
sandbox, and child-spawn stages under a bounded success-path poll. The trace requires both
an internal token and an absolute precreated regular file, caps output, ignores write
failures, records only static stage names, and is a no-op in release builds. Fresh spec-
compliance and code-quality reviews found no remaining issues after the success-path trace
race was removed.

Hosted run `29826648309` passed Validate and macOS completely. Its Windows job again failed
only the four inbound MCP lifecycle tests. Every failure recorded successful executor
admission, approval, worktree resolution, sandbox capability probing, shell resolution,
Windows process creation, Job assignment, primary-thread resume, and
`sandbox.child_spawn.complete`, but neither the absolute shell-entry marker nor the
heartbeat appeared. The command therefore resumed successfully but did not execute its
first shell builtin. The remaining MCP-specific startup difference is inherited stdin:
the noninteractive terminal child inherits the server's live JSON-RPC pipe while the
server concurrently reads requests from that pipe.

## Noninteractive Terminal Stdin Remediation (2026-07-21)

### Scope

Make the existing noninteractive `run_terminal` contract explicit at the sandbox process
boundary. Attach a null stdin handle to direct-shell and bubblewrap commands in both
streaming and collected-output paths so terminal workloads receive immediate EOF and
cannot read MCP protocol or approval input. Require each portable MCP lifecycle fixture to
observe EOF before creating its startup marker. Remove the temporary pre-shell trace hooks
after retaining their hosted evidence here. Preserve stdout and stderr streaming,
cancellation behavior, sandbox policy, Windows Job containment, and the public tool schema.

### Acceptance Criteria

- [x] Direct-shell and bubblewrap terminal commands receive a null stdin handle in both
  streaming and collected-output paths.
- [x] The four portable MCP lifecycle fixtures require immediate stdin EOF before starting
  their descendant workload.
- [x] Temporary pre-shell trace hooks and fixture plumbing are removed after their hosted
  diagnostic evidence is recorded.
- [x] Canonical local, documentation, and Windows-target validation gates pass.
- [x] The exact hosted Windows revision passes all four portable MCP lifecycle tests.
- [x] The exact hosted Linux, macOS, and Windows matrix is green.

### Affected Areas

`src/sandbox/mod.rs`, `src/tools/executor.rs`, `src/tools/core.rs`,
`src/sandbox/windows_job.rs`, `tests/mcp_integration.rs`, and FT-016 validation evidence.

### Validation Gates

The four portable lifecycle tests, `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`,
`git diff --check`, fresh spec-compliance and code-quality reviews, and exact-revision
hosted CI.

### Local Validation Evidence

The noninteractive-stdin revision passed `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. Both the standalone test gate and the complete test rerun inside
`task check` passed all 25 MCP integration tests, including the four portable lifecycle
regressions with their new EOF prerequisite. The first `task test` attempt encountered the
pre-existing Linux `crashed_supervisor_is_recovered_only_after_pid_namespace_exit` race;
that unrelated test passed on both subsequent canonical runs and does not execute the
modified terminal sandbox path. Native Windows behavior and the exact hosted matrix remain
open.

Hosted run `29829874790` passed Validate and macOS completely. Windows passed all 13 MCP
integration tests in 11.68 seconds, including targeted cancellation, stdin disconnect,
fatal input, and blocked-stdout cleanup with their immediate-EOF prerequisite. This
confirms that null terminal stdin removes the Git Bash startup stall and preserves Job
cleanup semantics. The Windows job later failed in the separate T003 session persistence
suite because its cross-process whole-directory replacement regression expected a live
sessions directory to be renameable, contrary to the existing Windows directory-pinning
contract. The full hosted matrix remains open while that platform-specific test contract
is corrected.

## Hosted Observation-Contention Remediation (2026-09-02)

Hosted run `33665599019` reached the Linux MCP integration suite after the complete
unit, CLI, installer, and credential-free live-report suites passed. The
`blocked_stdout_disconnect_reaps_terminal_descendants_on_every_platform` fixture then
failed while observing the successful `read_file` audit because its deliberately
500-millisecond observation lock acquisition overlapped the server's authoritative
`.skill-usage.lock` mutation. The process, backpressure, disconnect, and cleanup
assertions had not failed; the observer incorrectly treated one bounded lock timeout as
permanent instead of continuing within its existing ten-second progress deadline.

All MCP audit polling helpers now retry only `SessionError::InvalidMutation` carrying
the exact expected timeout text and exact observed `.skill-usage.lock` path. JSON
corruption, session-ID mismatch, other lock paths, and every other error still fail
immediately. Each synchronous attempt remains bounded to 500 milliseconds, the existing
five- or ten-second outer deadlines remain unchanged, and all original tool-completion,
cancellation, server-exit, heartbeat, and descendant-cleanup predicates remain required.
`task check` and `git diff --check` pass, and independent code-quality review found no
acceptance weakening. A replacement exact hosted matrix remains required.

## Post-Commit Completion-Fixture Budget (2026-09-02)

Hosted run `33671463213` passed Linux containment and static checks, then exposed that
the post-commit completion-wins-over-cancellation unit fixture inherited the deliberately
tight 250 ms generic test-only reconciliation budget. Under hosted load, authoritative
terminal-record reconciliation could exceed that fixture budget even though the
production boundary is four seconds. The fixture now selects an explicit five-second
test-only budget because it verifies terminal/cancellation ordering and absence of a
false cancellation audit, not deadline behavior. Production timeouts, lifecycle state
transitions, and audit semantics are unchanged.

## Superseded Historical Implementation Plan

1. Execute the Windows Job Object cancellation and disconnect suite on the configured
   runner.
2. Close the inherited FT-015 Git-configuration, unlink, and platform gates.
3. Rerun the canonical Task gates and two-stage review before moving FT-016 to `done/`.

## Historical Risks at This Stage

- A descendant that deliberately escapes the managed Unix process group remains outside
  this spec's guarantee and is owned by FT-017.
- Cancellation protocol publication depends on durable audit persistence; persistence
  failure must continue to fail closed rather than emit an unaudited cancellation.
- Windows Job Object runtime behavior has not been executed on this Linux host.


## Final Closure Evidence (2026-09-02)

This section supersedes earlier remaining-plan, current-risk, completion-state, and
native-evidence notes only where they described validation gates now executed. PR
[#25](https://github.com/skills-yaml/nib/pull/25) exact implementation run
[33683995100](https://github.com/skills-yaml/nib/actions/runs/33683995100)
passed the Validate, macOS Tests, and Windows Tests jobs for head
`c3b88564da4f6f654a8618e4fa544b353ece86f5` at clean merge checkout
`0479b72ad3d11fd7221632f042736b8489b6443b`. The matrix passed the complete
serial suites, Linux coverage at 85.87 percent (102,061/118,862), all native
all-target gates, exact release-binary qualification, and the Linux, macOS, and
Windows platform smokes.

The exact optimized binary hashes were
`e9b56b4c2b527ab04bd4e40932c83a632ae5bd5931010dee6152012b421e4276`
(Linux), `e7bbf6ea23d87a3e00b1447fc7880f2c93e6c67a27239f0068bcb599d18fb739`
(macOS), and
`e9250200aa0b06188e3e05d062ccd39115eb98311d0dc9b691cfdc5e9a324423`
(Windows). Local `task verify` also passed 1,062 library tests, 86 CLI tests,
every integration suite, and doctests during this reconciliation. All previously
open acceptance and validation items in this file are satisfied for its shipped
scope by this final matrix and the prior evidence recorded above.
