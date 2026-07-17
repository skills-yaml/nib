# FT-003: Hybrid Sandboxing for nib — Direct bwrap + Worktrees + Configurable Boundaries + Plan Gates

**Status:** Done
**Implementation track:** Rust only, delivered through [FT-005 Phase 5](ft_005_pure_rust_core_migration.md) and the reconciliation below.
**Related:** [FT-001: Basic Agent Tools](../development/ft_001_basic_agent_tools.md), [FT-002: Base Architecture](ft_002_base_architecture.md), [FT-005: Pure Rust Core Migration](ft_005_pure_rust_core_migration.md), [T001](T001_implement_core_agent_tools.md), `docs/tech/permissions.md`, `docs/tech/architecture.md`, `docs/specs/foundation/product.md`

## Historical Reopen Rationale

A 2026-07 audit found **no `sandbox/` implementation** in Python or Rust despite this
spec previously living in `done/`. That finding reopened the feature for a fresh Rust
implementation of bwrap + worktrees + boundaries + plan gates. The completed result is
documented in the reconciliation below.

The previous status "Done (merged 2026-06-20)" reflected merge intent, not verified
acceptance criteria. The historical functional criteria below are now backed by the
reconciliation evidence and completed repository-wide gates.

**Summary of hybrid philosophy**: Direct bwrap (OS-level isolation like Claude Code /
Codex) + git worktree composition (like Grok) + configurable boundaries (like Claude /
Antigravity) + strong Plan/approval gates (like Grok). Sessions and tool calls are
stored under the selected profile, normally `.nib/profiles/<id>/sessions/`. There is no
central projects/tasks database.

## Summary

nib's shipped **hybrid sandboxing architecture** combines the selected patterns from peer tools:

- **Direct bwrap** for OS-level namespace isolation (filesystem + process boundaries, like Claude Code and Codex).
- **Git worktree composition** for task-scoped isolation and safe parallel execution (like Grok).
- **Configurable boundaries** (allow/deny paths, network policies, profiles) inspired by Claude and Antigravity.
- **Strong Plan/approval gates** (Plan Mode + structured review + human approval before destructive steps, like Grok).

This hybrid keeps nib as the owner of the workload model and ToolExecutor while delivering defense-in-depth: kernel isolation (bwrap) + source control isolation (worktrees) + explicit policy (boundaries) + process controls (plans + approvals).

Direct bwrap remains the low-level engine. The shipped execution providers are
`internal`, `hybrid`, and `bwrap`; a full external agent CLI is not a core provider.

## Historical Problem Statement

At the FT-001 baseline, nib had:
- A solid outer permission and audit layer (`ToolExecutor`, `PermissionLevel`, workload recording, approval modes).
- Git worktree isolation for edit/execute operations.
- **Stub implementations** for the two highest-risk tools: `apply_patch` and `run_terminal`.

Pure approaches each have gaps:
- Pure bwrap (or delegating to `codex sandbox`) gives strong kernel isolation but can lack higher-level process controls and task-scoped source isolation.
- Pure worktree + approval gates (Grok style) are excellent for review but provide only weak kernel-level protection on raw commands.
- Configurable boundaries without enforcement are just documentation.

A hybrid is needed that gives nib:
- Hard OS-level isolation via direct bwrap (Claude/Codex pattern).
- Task-scoped, reversible isolation via git worktrees (Grok pattern).
- Explicit, auditable boundaries (Claude/Antigravity pattern).
- Structured human oversight via Plan Mode and approval gates (Grok pattern).

This matches nib's principles: workload model is sacred, defense-in-depth, leverage without full duplication, and human steerability.

## Goals

- Implement a **hybrid execution lane** in nib that combines:
  - Direct bwrap for kernel-level filesystem, process, and (optionally) network isolation.
  - Git worktree binding for task-scoped, git-reversible isolation and parallel sub-agents.
  - Configurable boundaries (allow/deny paths, network policies, named profiles) inspired by Claude and Antigravity.
  - Strong Plan/approval gates (Plan Mode exploration only, structured review, explicit approval before write/execute).
- Keep nib (ToolExecutor + session store) as the single source of truth for classification, approval, recording, and reconciliation (sessions stored in profile state under `.nib/`).
- Support graduated autonomy via `PermissionLevel` + session context + AGENTS.md rules.
- Make the hybrid the default recommended path on Linux while keeping fallbacks.
- Ensure full auditability: every bwrap invocation, worktree used, boundary applied,
  plan, and approval decision is recorded in the current profile session.
- Enable "Codex/Grok lane" patterns while staying true to nib's local-first, workload-sacred design.

## Non-goals

- Replacing nib's `ToolExecutor`, permission classification, workload model, or reconciliation logic.
- Depending on any full external agent CLI (Codex, full Claude, etc.) for core execution.
- Reimplementing bwrap itself.
- Requiring the full hybrid for read-only tools.
- Building a perfect unescapable sandbox (defense-in-depth + human gates are the real strategy).
- Changing the primary CLI/TUI of nib (this is an execution backend).

## Historical Proposed Design

### Hybrid Architecture for nib

nib will implement a **layered hybrid** that directly incorporates the strongest ideas from the compared tools:

1. **Plan / Approval Gate Layer** (Grok-style)
   - Explicit "Plan Mode" where the agent can read, search, and write only a structured plan.
   - All write/execute/destructive actions require explicit human review + approval (with clean diffs).
   - Plans and approvals are recorded as first-class artifacts in the workload model.

2. **Git Worktree Composition Layer** (Grok + native nib)
   - Before any potentially mutating execution, ToolExecutor creates an isolated git worktree for the task.
   - Sub-agents can run in their own worktrees (parallel safe execution).
   - The worktree becomes the primary writable mount inside the sandbox.

3. **Direct bwrap Isolation Layer** (Claude Code / Codex style)
   - Actual shell execution happens inside bwrap directly, without an external agent CLI.
   - The worktree is bound as the main writable area.
   - Host system is mostly read-only except for explicitly allowed paths.

4. **Configurable Boundaries Layer** (Claude + Antigravity style)
   - Filesystem allow/deny lists (paths the agent may read or write).
   - Network policy (off / restricted / allowlist).
   - Named profiles that expand into concrete bwrap flags + boundary sets.
   - Boundaries can be defined in project config, derived from AGENTS.md, or set per-task.

All four layers are orchestrated inside the existing `ToolExecutor`:
- `PermissionLevel` + session context drives profile + boundary selection.
- Approval decision (including plan approval) gates whether the hybrid envelope is even constructed.
- Every layer contributes to the audit record.

### Execution Flow Example

1. Task activated → optionally enter **Plan Mode**.
2. Agent explores using read-only tools.
3. Agent emits a structured plan → user reviews and approves (or iterates).
4. On "execute" approval:
   - Create git worktree.
   - Resolve boundaries + profile from config + `PermissionLevel` + context.
   - Call into `sandbox/bwrap`:
     - bwrap args include the worktree bind as writable root.
     - ro-binds + additional allow/deny from resolved boundaries.
     - `--unshare-*` flags per profile.
   - Command executes inside the bwrap + worktree envelope.
5. Results, diffs, and artifacts are reconciled back into the authoritative workload state.

### Concrete Components

- `src/sandbox/mod.rs` builds and runs bwrap commands from resolved boundaries.
- `src/config/mod.rs` validates base and named tighten-only boundary profiles.
- `src/session/mod.rs` persists plans and links them to later executions.
- Config schema example:

```toml
[execution]
provider = "hybrid"

[execution.hybrid]
default_profile = "restricted"
plan_mode = true

[execution.boundaries]
allow_write = [".", "./build", "~/.cache/tool-specific"]
network = "restricted"
```

### Updates to Existing Components

- `src/config/mod.rs`: execution providers, boundaries, and named profiles.
- `src/tools/executor.rs`: hybrid orchestration, worktree selection, approval, and audit.
- `src/sandbox/`: bwrap execution and worktree ownership.
- CLI/TUI runtime surfaces: plan approval and execution status without separate
  `nib plan` or `nib sandbox-test` commands.
- Context assembly: include current boundaries, active plan, and worktree status when relevant.
- Profile sessions: plans, boundary snapshots, and full hybrid invocation details.

### Detection, Fallback, and Output Handling

- Detect bwrap availability; explicit `bwrap` fails closed, while `hybrid` falls back to
  `internal` only when configured boundaries permit direct execution.
- Always capture output, apply nib redaction, and record the complete hybrid context (provider, bwrap args, worktree, boundaries, plan ID, approval) in the `ToolExecutionRecord`.
- `nib doctor` surfaces active hybrid capabilities.

## Alternatives Considered

| Approach                        | Pros                                           | Cons                                              | Decision |
|---------------------------------|------------------------------------------------|---------------------------------------------------|----------|
| **Hybrid (direct bwrap + worktrees + boundaries + plan gates)** | Best of all worlds: kernel isolation + git isolation + policy + human control | More implementation surface | Recommended |
| Pure direct bwrap               | Simpler than full hybrid                       | Lacks strong process gates                        | Fallback |
| Delegate to `codex sandbox`     | Fast, reuses tested profiles                   | Less control, external dep                        | Rejected for core provider |
| Pure Grok-style (plan + worktrees only) | Excellent review flow                          | Weaker kernel protection on commands              | Complementary |
| Full Docker / microVM           | Very strong isolation                          | Heavy, slow for everyday use                      | Special high-trust cases |

## Risks and Tradeoffs

- **Implementation complexity**: Combining four layers requires careful orchestration. Mitigation: Start with conservative defaults, add one layer at a time, comprehensive tests.
- **User experience**: More modes (Plan vs Execute) and config can feel heavier initially. Mitigation: Excellent defaults + clear TUI/CLI guidance.
- **Bypass risk**: No sandbox is perfect (see real-world bypasses in Claude and Antigravity). Mitigation: defense-in-depth (bwrap + worktree + boundaries + plan gates + human review).
- **Environment & compatibility**: Same issues as pure bwrap. Mitigation: reference patterns from Codex + incremental allowlists.
- **Cross-platform**: bwrap is Linux-focused. Mitigation: document macOS/Windows paths (Seatbelt, containers, or external VMs) for future providers.

## Historical Rollout Plan (completed)

1. **Phase 1**: Core hybrid plumbing (worktree + direct bwrap + basic boundaries) + Plan Mode skeleton. Update `run_terminal`.
2. **Phase 2**: Full boundary resolution, named profiles, strong approval integration, and workload recording.
3. **Phase 3**: Polish (TUI support, doctor checks, AGENTS.md integration, tests).
4. **Phase 4**: Optional delegation paths and cross-platform providers.

`provider = "hybrid"` is the documented default. `internal` remains the bounded fallback
when configured restrictions permit direct execution.

## Historical Acceptance Snapshot

This checklist records feature closeout; current evidence and platform limits are in
the reconciliation below.

- [x] Hybrid mode works: Plan Mode → approval → worktree creation → bwrap execution with boundaries enforced.
- [x] Writable access limited to the worktree + explicitly allowed paths (tests verify).
- [x] Plans and approvals are persisted and linked to executions in the workload model.
- [x] Every layer (bwrap flags, worktree, boundaries, plan) is recorded in `ToolExecutionRecord`.
- [x] AGENTS.md and `PermissionLevel` can influence the active profile/boundaries.
- [x] Fallbacks function cleanly.
- [x] `task check` + `task test` pass + dedicated hybrid isolation tests.
- [x] End-to-end: complex task goes through plan → review → approved hybrid execution → reconciled state.
- [x] Docs (architecture, permissions, this spec) reflect the hybrid approach.

## Historical Open Questions

- Exact richness of initial boundary profiles and how aggressively to auto-detect tool caches.
- Whether to implement lightweight shell snapshot compatibility (inspired by Codex).
- macOS/Windows equivalent strategies.
- How deeply Plan Mode integrates with the TUI kanban views.
- Interaction with future MCP exposure of the hybrid lane.

---

**Implementation follow-through:** T009 supplied the Rust/config foundation; the
hybrid layers and verification evidence were completed through the reopened audit and
reconciliation below.

## Reopened Audit (2026-07-15)

Scope: make bwrap capability detection executable, fail closed on isolation errors,
persist plan/approval/boundary evidence, apply AGENTS/skill constraints, and prove
fallback plus hybrid execution.

Affected areas: `src/sandbox/`, `src/tools/executor.rs`, `src/config/`,
`src/session/`, and sandbox/runtime E2E tests.

Validation gates: dedicated allowed/denied/fallback tests, plan-linked E2E,
`nib doctor`, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Compose approved plans, session worktrees, argument-aware permissions, configurable
boundaries, executable bwrap detection, and a documented hybrid fallback.

### Acceptance Criteria

- [x] Mutating tools require an approved persisted plan and a session worktree.
- [x] Usable bwrap enforces worktree/allowed-write and network boundaries; explicit `bwrap` fails closed when unavailable.
- [x] Execution audit records provider, profile, bwrap args, boundaries, worktree, approval, and plan ID.
- [x] Hybrid fallback is allowed only when configured boundaries permit direct execution.
- [x] AGENTS policies can allow, deny, or require approval; active-skill constraints
  can only deny or require approval before worktree creation.
- [x] AGENTS rules dynamically select configured, tighten-only named boundary profiles and fail closed on invalid, unknown, conflicting, or weaker selections.
- [x] The 2026-07-15 local aggregate gates passed.

### Affected Areas

`src/sandbox/`, `src/sandbox/worktree.rs`, `src/tools/executor.rs`, `src/config/`,
`src/session/`, and sandbox/runtime tests.

### Implementation Evidence

`src/sandbox/mod.rs` owns capability probing, bwrap arguments, fallback, output bounds,
and process cleanup. `src/config/mod.rs` validates additive
`execution.boundary_profiles` overlays against the configured base boundary.
`src/tools/executor.rs` resolves `nib-boundary: profile <name>`, upgrades an internal
provider to hybrid (or bwrap for disabled network), and composes the selected name and
resolved boundaries through plan, approval, worktree, dispatch, and audit layers.

### Validation Evidence

`tests/sandbox_integration.rs::test_sandbox_write_restrictions`, executor plan/policy
tests, and `tests/test_runtime_e2e.rs::approved_patch_physically_changes_only_the_session_worktree_and_is_verified`.
`config::tests::named_boundary_profiles_roundtrip_and_only_tighten_the_base_boundary`
and `named_boundary_profile_validation_rejects_weaker_or_reserved_profiles` prove the
TOML contract. `tests/executor.rs::agents_named_boundary_profile_preserves_approval_worktree_and_audit`
proves approved-plan, provider, profile, boundary, and worktree persistence from an
internal baseline; `invalid_unknown_or_weaker_agents_boundary_profiles_fail_before_approval_and_worktree`
proves fail-closed denial before side effects.

### Validation Gates

- [x] Dedicated allowed/denied/fallback and plan-linked E2E tests exist.
- [x] Dynamic AGENTS-to-boundary-profile selection and fail-closed regression tests.
- [x] Linux `nib doctor`, `task check`, and `task test` passed during reconciliation.

### Documented Platform Boundary

macOS/Windows kernel isolation equivalents remain future work; Git worktrees and
policy still apply. This platform boundary is documented and is not an incomplete
Linux acceptance criterion.
