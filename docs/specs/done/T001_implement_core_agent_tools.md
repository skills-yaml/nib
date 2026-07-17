# T001: Implement Core Agent Tools (FT-001)

**Status:** Done

**Related Feature:** [FT-001: Basic Agent Tools Implementation](../development/ft_001_basic_agent_tools.md)

> Historical proposal note: sections before the 2026-07-15 reconciliation preserve
> the original Python/global-Task design and delivery sequence. The Rust,
> profile-session implementation described in the reconciliation is authoritative.

## Goal

Build the first version of nib's minimal, safe, auditable core tool surface so that the agent can perform real coding work while respecting workload ownership, permissions, MCP, Skills, and AGENTS.md.

## Scope

Implement the five core tools defined in FT-001, the central ToolExecutor, basic permission enforcement, worktree helpers, workload recording, and the initial MCP + Skills + AGENTS.md integration points for tooling.

Out of scope for this task (will be separate tasks):
- Full rich TUI for approvals
- Production-grade smart approval classifier
- Complete MCP server (beyond basic exposure)
- Advanced skill execution (beyond instruction injection + simple wrappers)

## Success Criteria

- The five tools (`read_file`, `list_directory`, `grep`, `apply_patch`, `run_terminal`) are implemented and pass their unit/integration tests.
- Tool usage is always recorded against the current Task in the workload store.
- Path scoping + worktree isolation is enforced for write/execute operations.
- Approval modes (at minimum manual) work for destructive commands.
- Context loaded from AGENTS.md is visible/consulted during tool planning and execution.
- At least one Skill can influence tool usage (via instructions or a simple wrapper).
- The tools are callable both directly and via the MCP layer (client + basic server exposure).
- `task check` and `task test` pass.
- A small end-to-end demo exists (e.g. "use tools inside a worktree to make a change, record it, and reconcile").

## Suggested Implementation Order (high level)

1. Tool registry + Pydantic metadata + executor skeleton (with logging to workload).
2. Safe read-only tools (`read_file`, `list_directory`, `grep`).
3. `apply_patch` + worktree creation/cleanup helpers.
4. `run_terminal` with classification + approval hook.
5. Integration of the existing `context/` loader (AGENTS.md + skills) into the executor.
6. Basic MCP wrapping in `integrations/mcp.py`.
7. Tests + demo + documentation updates.

## Dependencies

- FT-001 spec (this task implements it).
- Existing scaffolding: `context/`, `skills/`, `integrations/mcp.py`, workload models.
- Python environment (already set up with uv, ruff, pyright, pytest).

## Risks / Notes

- Keep the initial implementation simple and testable (function-based tools + one central executor).
- Do not over-engineer the patch format or approval UI in the first pass.
- Every tool call must be linkable back to a Task ID.

## Exit Criteria

All success criteria above are met, the code follows the rules in `AGENTS.md` and `docs/tech/*`, and a PR description or commit message references this task and FT-001.

---

**Owner:** nib team (feat/implement-basic-agent-tools)  
**Estimate:** 3–6 focused sessions  
**Implementation note:** The Rust registry, executor, worktree, tools, audit, context
integration, and E2E edit path are present. Current completion evidence is reconciled below.

**Post-execution notes (2026-06 snapshot):**
Implementation followed the suggested order closely. At that snapshot, expanded
tests and write-tool behavior were still open; the later reconciliation below records
their Rust implementation and final evidence. No separate `.plan.md` was required for
the initial pass.

## Reopened Audit (2026-07-15)

Scope: finish the five tool interfaces, fail-closed scope/worktree enforcement,
session/task-linked audit, policy enforcement, MCP exposure, and observable tests.

Affected areas: `src/tools/`, `src/sandbox/`, `src/session/`, `src/context/`,
`src/integrations/`, `src/tui/`, and tool/runtime integration tests.

Acceptance criteria: every criterion in the Success Criteria and Exit Criteria
sections is backed by an observable test or verified runtime artifact.

Validation gates: focused success/error/denial/worktree tests, a real edit and
reconciliation E2E, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

The shipped scope is the Rust core-tool registry and dispatcher, central gated
executor, worktree/sandbox execution, profile-session audit, context policy, and MCP
exposure. Historical Python/Pydantic paths and a global Task row are superseded.

### Acceptance Criteria

- [x] `read_file`, `list_directory`, `grep`, `apply_patch`, and `run_terminal` have
  bounded schemas and real Rust implementations.
- [x] Agent-selected calls pass through scope, policy, approval, dispatch, redaction,
  and profile-session audit in `ToolExecutor`.
- [x] Mutations require an approved persisted plan and execute in a session worktree.
- [x] Core tools are exposed through the stdio MCP server without bypassing the executor.
- [x] AGENTS and selected-skill rules can alter approval or deny execution.
- [x] Final repository gates are green after all concurrent remediation.

### Affected Areas

`src/tools/`, `src/sandbox/`, `src/session/`, `src/context/`, `src/agent/`,
`src/integrations/mcp_server.rs`, and executor/runtime integration tests.

### Implementation Evidence

- `src/tools/registry.rs`, `src/tools/core.rs`, and `src/tools/executor.rs` own schemas,
  implementations, approval/policy, dispatch, and `ToolCallRecord` audit.
- `src/agent/loop.rs` supplies bounded context and reconciles observations into the
  persisted session plan; `src/sandbox/worktree.rs` owns session worktrees.

### Validation Evidence

- `tests/executor.rs`: `read_only_call_succeeds_and_outside_root_attempt_is_audited`,
  `mutating_tools_require_an_approved_plan`, and
  `patch_defaults_to_dry_run_and_terminal_nonzero_is_failure`.
- `tests/test_runtime_e2e.rs`:
  `approved_patch_physically_changes_only_the_session_worktree_and_is_verified` and
  `selected_skill_can_require_tool_approval_and_denial_has_no_side_effect`.
- `tests/test_runtime_e2e.rs`:
  `mcp_delegation_is_permission_gated_dispatched_and_audited_over_stdio`.

### Validation Gates

- [x] Focused tool success, denial, scope, worktree, and MCP integration tests exist.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

The original global Task-ID requirement was superseded. The authoritative equivalent
is a profile-scoped session, its `PlanStep`, lifecycle events, and durable task
records; no in-scope implementation gap remains.

## Reopened Audit (2026-07-16)

### Scope

Close the remaining sessionless execution path so every production `ToolExecutor`
dispatch either records a profile-session audit or fails before the tool runs.

### Acceptance Criteria

- [x] `ToolExecutor::execute` creates or resolves an authoritative audit session when
  callers do not provide one.
- [x] Successful and denied sessionless calls persist attempt and result records.
- [x] Doctor permission probes remain functional and auditable.
- [x] No production call can return a successful tool result without a durable audit.

### Affected Areas

`src/tools/executor.rs`, `src/doctor.rs`, session persistence, and executor tests.

### Implementation Plan

1. Resolve or create a profile-scoped session before any sessionless dispatch.
2. Reuse that implicit audit session for the lifetime of the executor and fail closed
   if session creation or attempt recording fails.
3. Prove successful and denied probes, including doctor, persist complete audit records.

### Risks

Implicit audit sessions add local state for diagnostic callers. Reusing one session per
executor bounds that growth, while mandatory pre-dispatch attempt persistence prevents
an audit setup failure from becoming an unaudited action.

### Completion Evidence

`tests/executor.rs::sessionless_read_is_redacted_and_persisted_in_an_implicit_audit_session`
proves mandatory implicit-session audit. Doctor tests prove successful and denied
permission probes share that audit session. Sessionless audit context remains
non-authoritative for schedule/background ownership, and caller-provided plan IDs
cannot forge persisted audit linkage.

### Validation Gates

Focused sessionless success/denial/audit tests, `task check`, `task test`, and
`task coverage`.
