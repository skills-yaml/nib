# T020: MCP Client Integration

**Status:** Development
**Related:** [FT-005](../done/ft_005_pure_rust_core_migration.md), [Ecosystem Integration](../../tech/ecosystem_integration.md)

## Scope

nib needs to connect to MCP (Model Context Protocol) servers to expose their tools to the internal agent loop.

## Acceptance Criteria

- [x] Create `src/integrations/mcp.rs`.
- [x] Implement a basic MCP client manager that reads strict project `.nib/config.toml` server configuration.
- [x] Expose discovered MCP tools through the `ToolExecutor` / `registry`.
- [x] Ensure MCP tool execution routes through the MCP client but still triggers `ToolExecutor` approval/recording logic.

## Affected Areas

- `src/integrations/mcp.rs`
- `src/config/mod.rs` (if adding MCP server config)
- `src/tools/executor.rs` and `src/tools/registry.rs` (dynamic tools vs static tools)

## Validation Gates

- Must pass `task check` and `task test`.
- Demonstrate one mock MCP tool being called.

## Reopened Audit (2026-07-15)

Scope: remove the Python fixture dependency, validate configured lifecycle/timeouts,
normalize schemas/permissions, and prove MCP calls are approved and audited through
ToolExecutor.

Affected areas: `src/integrations/mcp.rs`, config/runtime initialization, executor
routing, Rust mock fixture, and MCP integration tests.

Validation gates: Rust-only discovery/call/error/approval/audit tests, `task check`,
and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Load strict project MCP stdio server configuration, manage child lifecycle and
request correlation, normalize bounded tool schemas, and route calls through normal
executor approval and audit.

### Acceptance Criteria

- [x] `src/integrations/mcp.rs` implements a configured stdio client manager.
- [x] Tools are discovered, namespaced as `server::tool`, and schema bounded.
- [x] Executor routing preserves classification, approval, result redaction, and session audit.
- [x] Request timeout, cancellation, dropped callers, child cleanup, and ambient-secret stripping fail safely.
- [x] A Rust mock MCP process demonstrates discovery and calls without Python fixtures.
- [x] Fresh local aggregate gates and Linux release-binary smoke are green.
- [ ] Windows and macOS runtime gates are green.

### Affected Areas

`src/integrations/mcp.rs`, `src/integrations/mcp_framing.rs`, `src/config/`,
`src/tools/executor.rs`, `src/agent/loop.rs`, and MCP tests.

### Implementation Evidence

- `src/integrations/mcp.rs` owns stdio children, pending requests, discovery, calls,
  schema validation, timeouts, and cleanup.
- `src/integrations/mcp_framing.rs` owns bounded JSON/line framing.

### Validation Evidence

- `src/integrations/mcp.rs`: schema/count bounds, timeout, abort, invalid config,
  environment stripping, and descendant termination tests.
- `tests/mcp_integration.rs::test_mcp_mock_server` and
  `tests/test_runtime_e2e.rs::mcp_delegation_is_permission_gated_dispatched_and_audited_over_stdio`.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
remediation gates below are authoritative for completion.

- [x] Rust-only discovery, call, timeout, cancellation, approval, and audit tests exist.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

HTTP/SSE MCP transports and OAuth are not implemented; they are future scope, not a
claim of the stdio v1 client. The lifecycle, secret-boundary, and managed-worktree
remediations below supersede this earlier assessment of the remaining in-scope work.

## Final Secret-Boundary Review Remediation (2026-07-15)

### Scope

Apply configured sensitive-value redaction before MCP client initialization can return
or print any server-controlled error, including JSON-RPC initialization failures and
child stderr.

### Acceptance Criteria

- [x] MCP manager/client initialization receives the resolved configured sensitive
  values before the first child request and redacts server-controlled RPC/transport
  errors before they reach CLI, agent, doctor, gateway, or audit callers.
- [x] MCP child stderr cannot bypass the same secret boundary during startup or later
  transport failure.
- [x] Deterministic mock-server regressions echo configured secrets through RPC errors
  (including quoted, escaped, and multiline values) and stderr and prove the raw or
  JSON-escaped values never appear in returned or inherited output.
- [x] The first fatal reader or writer failure linearizes with outbound enqueue, closes
  the transport to new and queued requests, resolves every pending request once, and
  terminates group-contained descendants while reaping the unusable direct MCP child.
- [x] Existing request timeout, cancellation, correlation, and child cleanup behavior
  remains intact.
- [ ] Windows MCP children are assigned before execution to a kill-on-close Job Object,
  so timeout, cancellation, manager drop, and fatal transport terminate descendants as
  well as the direct child; the configured Windows CI job executes the regression.

### Affected Areas

`src/integrations/mcp.rs`, `src/sandbox/mod.rs`, runtime/doctor MCP construction,
target-specific dependency metadata, MCP fixtures, and client integration tests.

### Validation Gates

Focused MCP initialization/error/child-output, fatal enqueue race, process-group cleanup,
and Windows Job Object tests; `task test`, `task check`, `task coverage`, Windows CI
`task test`, and isolated release binary smoke.

## Exact-Ownership Transport Remediation (2026-07-15)

### Scope

Make outbound MCP initialization a delivered transport transaction rather than a queue
acceptance, enforce sensitive-value normalization at every callable client boundary, and
prove lifecycle ownership on both Unix and Windows. Shared Git worktree execution used by
MCP-routed gated tools must remain bounded, hook-disabled, contained, and compensating.

### Acceptance Criteria

- [x] Outbound notifications carry a write acknowledgment (or equivalent transport health
  barrier), and `notifications/initialized` cannot succeed unless its full frame was
  written to a live child transport.
- [x] A deterministic child-exit/write-failure race proves client startup fails and joins
  the supervisor instead of returning a healthy client after queue acceptance.
- [x] Direct `McpServerClient` construction is crate-private behind `McpManager`, or it
  normalizes raw, JSON, debug, quoted, escaped, and multiline sensitive spellings before
  starting the child; every returned error variant is redacted at that boundary.
- [x] Manager startup, ordinary drop, initialization failure, fatal transport, queued
  request drainage, direct-root-exit detection, and descendant cleanup have
  platform-independent code paths and deterministic Linux runtime tests.
- [ ] Windows Job Object lifecycle and descendant cleanup tests compile and execute in
  Windows CI where process execution is available.
- [x] MCP-routed mutating tool worktrees use bounded, hook-disabled managed Git execution.
  Post-add compensation removes only exact-owned path, registration, and branch state;
  an unproven registration is preserved and reported rather than destructively guessed.
- [x] Managed-worktree completion inherits FT-015's exact registration-creation and
  branch-lineage provenance plus cross-process receipt recovery.
- [x] Managed-worktree completion inherits FT-015's supported Unix exact-namespace
  detachment, fail-closed residual cleanup reporting, and durable receipt recovery;
  Windows runtime execution remains covered by the platform criteria around it.
- [ ] Synchronous managed commands terminate their Windows Job Object descendants and
  reap the direct child on timeout and cancellation, matching the Unix process-group
  boundary.

### Affected Areas

`src/integrations/mcp.rs`, `src/integrations/worktree.rs`,
`src/sandbox/worktree.rs`, `src/sandbox/mod.rs`, `src/sandbox/windows_job.rs`, MCP
manager construction boundaries, transport fixtures, and cross-platform lifecycle tests.

### Validation Gates

Focused startup-delivery, redaction, manager drop/fatal, worktree compensation, and
Windows containment tests; `cargo fmt --all -- --check`, strict all-target Clippy,
`git diff --check`, and canonical Task/Windows CI gates before completion.

## Successful Metadata Secret-Boundary Remediation (2026-07-15)

### Scope

Extend the configured sensitive-value boundary to successful `tools/list` responses.
Server-controlled tool identifiers, descriptions, and input schemas must not carry a
configured credential or generic secret into the executor registry, planner request,
session audit, diagnostics, or returned discovery result.

### Acceptance Criteria

- [x] MCP discovery validates the complete serialized tool metadata before registering
  it and rejects secret-bearing metadata without returning the offending identifier,
  description, schema fragment, raw secret, or encoded secret spelling.
- [x] Rejection is atomic per configured server: no subset of a malicious server's tool
  list remains callable or visible after metadata validation fails.
- [x] A deterministic mock server places configured raw, quoted, escaped, and multiline
  secret spellings in successful tool names, descriptions, schema keys, and schema
  values; manager construction fails with a bounded redacted error.
- [x] A runtime/executor regression proves accepted MCP metadata entering model context
  contains none of the configured sensitive values while ordinary discovery and calls
  remain functional.
- [x] The boundary validates the final namespaced representation, including configured
  server names and manager-added fields, so embedded generic-token spellings cannot enter
  model context through either server metadata or nib's namespace prefix.
- [x] Sensitive spellings are normalized once into one shared, count- and byte-bounded
  matcher. Discovery scans each bounded serialized metadata payload without per-node
  cloning or repeated sorting, and excessive matcher state fails with a constant bounded
  diagnostic before any server metadata is registered.

### Affected Areas

`Cargo.toml`, `src/integrations/mcp.rs`, executor/registry model-context preparation,
MCP fixtures, and client/runtime integration tests.

### Validation Gates

Focused successful-discovery redaction and atomic-registration tests, existing MCP
transport/call tests, `task test`, `task check`, `task coverage`, and final two-stage
review.

## Local Validation Evidence (2026-07-16)

Secret/error normalization, initialized write acknowledgment, fatal enqueue/drain,
manager drop and initialization cleanup, direct-root-exit inherited-stdio cleanup,
atomic metadata rejection, final namespace validation, and the shared bounded matcher
are covered by deterministic `integrations::mcp` and `mcp_integration` tests. The full
17-test MCP process suite passed, including the failed-add preservation and audit-lock
liveness regressions.

The reconciled Linux tree passed `task check`, `task test` (772 top-level tests),
`task coverage` at 83.94 percent (53,734/64,015), `task docs:check`, `task build`, strict
all-target check/Clippy, and isolated release-binary MCP initialize/list/call/error and
size-bound smoke. Windows and macOS runtime evidence and the inherited FT-015 ownership
gates remain open.

## Remaining Implementation Plan

1. Retain FT-015's documented isolation boundary: mutable same-UID Git interference is
   outside the threat model, while every attributable cleanup continues to require
   exact persisted ownership and fail-closed compensation. No stronger arbitrary-peer
   or pathname-unlink claim is made by T020.
2. Execute the Windows Job Object and macOS MCP lifecycle suites on their configured
   runners and retain the resulting runtime evidence.
3. Rerun the canonical Task gates and two-stage review before moving T020 to `done/`.

## Current Risks

- Same-UID repository/worktree Git-configuration mutation is the explicit inherited
  FT-015 trust boundary; T020 does not classify it as an unimplemented containment
  guarantee.
- A descendant that deliberately escapes the managed Unix process group is outside this
  spec's guarantee and remains owned by FT-017.
- Windows Job Object and macOS MCP behavior have not been executed on this Linux host.
