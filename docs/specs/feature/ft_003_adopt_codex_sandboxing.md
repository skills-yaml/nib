# FT-003: Hybrid Sandboxing for nib — Direct bwrap + Worktrees + Configurable Boundaries + Plan Gates

**Status:** Draft  
**Related:** [FT-001: Basic Agent Tools](../feature/ft_001_basic_agent_tools.md), [FT-002: Base Architecture](../feature/ft_002_base_architecture.md), [T001](../task/T001_implement_core_agent_tools.md), `docs/tech/permissions.md`, `docs/tech/architecture.md`, `docs/specs/foundation/product.md`

**Summary of hybrid philosophy**: Direct bwrap (OS-level isolation like Claude Code / Codex) + git worktree composition (like Grok) + configurable boundaries (like Claude / Antigravity) + strong Plan/approval gates (like Grok). Sessions (conversations + tool calls) are stored as files in the project's `.nib/sessions/`. No central projects/tasks model.

## Summary

nib should adopt a **hybrid sandboxing architecture** that combines the best patterns from peer tools:

- **Direct bwrap** for OS-level namespace isolation (filesystem + process boundaries, like Claude Code and Codex).
- **Git worktree composition** for task-scoped isolation and safe parallel execution (like Grok).
- **Configurable boundaries** (allow/deny paths, network policies, profiles) inspired by Claude and Antigravity.
- **Strong Plan/approval gates** (Plan Mode + structured review + human approval before destructive steps, like Grok).

This hybrid keeps nib as the owner of the workload model and ToolExecutor while delivering defense-in-depth: kernel isolation (bwrap) + source control isolation (worktrees) + explicit policy (boundaries) + process controls (plans + approvals).

Direct bwrap remains the low-level engine; delegation to `codex sandbox` is a secondary/compatibility option.

## Problem Statement

nib's current implementation (as of FT-001) has:
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
- Keep nib (ToolExecutor + session store) as the single source of truth for classification, approval, recording, and reconciliation (sessions stored in .nib/).
- Support graduated autonomy via `PermissionLevel` + session context + AGENTS.md rules.
- Make the hybrid the default recommended path on Linux while keeping fallbacks.
- Ensure full auditability: every bwrap invocation, worktree used, boundary applied, plan, and approval decision is recorded in the current session file under .nib/sessions/.
- Enable "Codex/Grok lane" patterns while staying true to nib's local-first, workload-sacred design.

## Non-goals

- Replacing nib's `ToolExecutor`, permission classification, workload model, or reconciliation logic.
- Depending on any full external agent CLI (Codex, full Claude, etc.) for core execution.
- Reimplementing bwrap itself.
- Requiring the full hybrid for read-only tools.
- Building a perfect unescapable sandbox (defense-in-depth + human gates are the real strategy).
- Changing the primary CLI/TUI of nib (this is an execution backend).

## Proposed Design

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
   - Actual shell execution happens inside bwrap (direct call from Python, not via `codex sandbox`).
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

- `src/nib/sandbox/bwrap.py` — builds and runs the bwrap command, accepts worktree + boundary descriptors.
- `src/nib/sandbox/boundaries.py` — resolves configurable allow/deny rules and named profiles into bwrap flags.
- Plan support (extend core or new `plan.py`) — stores plans, links them to tasks and later executions.
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

- `tools/models.py`: `ExecutionProvider`, `SandboxProfile`, `BoundarySet`, `Plan`.
- `tools/core_tools.py` and `tools/executor.py`: Hybrid orchestration — create worktree, resolve boundaries, call bwrap layer, record everything.
- New `sandbox/` package: `bwrap.py`, `boundaries.py`, `policy.py`.
- CLI: `nib plan`, `nib approve`, `nib sandbox-test`, config commands.
- Context assembly: include current boundaries, active plan, and worktree status when relevant.
- Workload store: new fields for plans, boundary snapshots, and full hybrid invocation details.

### Detection, Fallback, and Output Handling

- Detect bwrap availability; fall back gracefully to "internal" or "codex" provider.
- Always capture output, apply nib redaction, and record the complete hybrid context (provider, bwrap args, worktree, boundaries, plan ID, approval) in the `ToolExecutionRecord`.
- `nib doctor` (future) surfaces the active hybrid capabilities.

## Alternatives Considered

| Approach                        | Pros                                           | Cons                                              | Decision |
|---------------------------------|------------------------------------------------|---------------------------------------------------|----------|
| **Hybrid (direct bwrap + worktrees + boundaries + plan gates)** | Best of all worlds: kernel isolation + git isolation + policy + human control | More implementation surface | Recommended |
| Pure direct bwrap               | Simpler than full hybrid                       | Lacks strong process gates                        | Fallback |
| Delegate to `codex sandbox`     | Fast, reuses tested profiles                   | Less control, external dep                        | Compatibility option |
| Pure Grok-style (plan + worktrees only) | Excellent review flow                          | Weaker kernel protection on commands              | Complementary |
| Full Docker / microVM           | Very strong isolation                          | Heavy, slow for everyday use                      | Special high-trust cases |

## Risks and Tradeoffs

- **Implementation complexity**: Combining four layers requires careful orchestration. Mitigation: Start with conservative defaults, add one layer at a time, comprehensive tests.
- **User experience**: More modes (Plan vs Execute) and config can feel heavier initially. Mitigation: Excellent defaults + clear TUI/CLI guidance.
- **Bypass risk**: No sandbox is perfect (see real-world bypasses in Claude and Antigravity). Mitigation: defense-in-depth (bwrap + worktree + boundaries + plan gates + human review).
- **Environment & compatibility**: Same issues as pure bwrap. Mitigation: reference patterns from Codex + incremental allowlists.
- **Cross-platform**: bwrap is Linux-focused. Mitigation: document macOS/Windows paths (Seatbelt, containers, or external VMs) for future providers.

## Rollout Plan

1. **Phase 1**: Core hybrid plumbing (worktree + direct bwrap + basic boundaries) + Plan Mode skeleton. Update `run_terminal`.
2. **Phase 2**: Full boundary resolution, named profiles, strong approval integration, and workload recording.
3. **Phase 3**: Polish (TUI support, doctor checks, AGENTS.md integration, tests).
4. **Phase 4**: Optional delegation paths and cross-platform providers.

`provider = "hybrid"` becomes the documented recommended default once mature. "internal" remains the safe fallback.

## Validation and Acceptance Criteria

- [ ] Hybrid mode works: Plan Mode → approval → worktree creation → bwrap execution with boundaries enforced.
- [ ] Writable access limited to the worktree + explicitly allowed paths (tests verify).
- [ ] Plans and approvals are persisted and linked to executions in the workload model.
- [ ] Every layer (bwrap flags, worktree, boundaries, plan) is recorded in `ToolExecutionRecord`.
- [ ] AGENTS.md and `PermissionLevel` can influence the active profile/boundaries.
- [ ] Fallbacks function cleanly.
- [ ] `task check` + `task test` pass + dedicated hybrid isolation tests.
- [ ] End-to-end: complex task goes through plan → review → approved hybrid execution → reconciled state.
- [ ] Docs (architecture, permissions, this spec) reflect the hybrid approach.

## Open Questions

- Exact richness of initial boundary profiles and how aggressively to auto-detect tool caches.
- Whether to implement lightweight shell snapshot compatibility (inspired by Codex).
- macOS/Windows equivalent strategies.
- How deeply Plan Mode integrates with the TUI kanban views.
- Interaction with future MCP exposure of the hybrid lane.

---

**Next steps**: Create T009 (or equivalent) task spec focused on implementing the hybrid layers (starting with bwrap + worktree composition + minimal Plan gate).

Update this spec as the hybrid design or boundary model evolves.