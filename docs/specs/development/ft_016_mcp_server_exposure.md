# FT-016: MCP Server Exposing the Agent Loop

**Status:** Development
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
- [ ] Windows runtime gates are green.

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
- [ ] Windows terminal and agent children are contained in a kill-on-close Job Object
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
- [ ] Windows synchronous process execution assigns the child to a kill-on-close Job
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

## Remaining Implementation Plan

1. Execute the Windows Job Object cancellation and disconnect suite on the configured
   runner.
2. Close the inherited FT-015 Git-configuration, unlink, and platform gates.
3. Rerun the canonical Task gates and two-stage review before moving FT-016 to `done/`.

## Current Risks

- A descendant that deliberately escapes the managed Unix process group remains outside
  this spec's guarantee and is owned by FT-017.
- Cancellation protocol publication depends on durable audit persistence; persistence
  failure must continue to fail closed rather than emit an unaudited cancellation.
- Windows Job Object runtime behavior has not been executed on this Linux host.
