# T007: Configuration Schema Alignment + "nib doctor" Validation

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

## Problem Statement

nib lacks a unified, extensible configuration schema aligned with advanced agent runtimes (covering model, agent bounds, terminal, compression, memory, approvals, etc.). Current config is scattered (pyproject, placeholders). Additionally, there is no system introspection/validation tool equivalent to "doctor" that MUST pass with exit code 0, ensuring the runtime is healthy before execution. This leads to brittle setups and hard-to-debug issues in production-like agent deployments.

## Goals

- Define and implement a complete config schema (YAML/TOML) covering all engine aspects: model/provider/context_length, agent/max_turns/tool_enforcement, terminal/backend/timeout, compression, memory, approvals.mode, workload, mcp, skills.
- Add "nib doctor" (CLI command) that validates config, environment, permissions, skills/MCP connectivity, and runtime readiness (MUST exit 0 on success).
- Integrate with T003 (compression/memory), T005 (state machine), T006 (MCP/skills), T004 (daemons).
- Support profiles (T004) for per-workspace overrides.

## Non-Goals

- GUI config editor (CLI + file-based for v1).
- Remote config management.

## Proposed Design

- Config in `~/.nib/config.yaml` (or project-local .nib/config.yaml), parsed with Pydantic.
- Schema sections as in T002.
- `nib doctor` command: Checks:
  - Config validity and required fields.
  - Git/worktree availability.
  - MCP server reachability.
  - Skills discoverable.
  - Workload DB writable.
  - Permission layers functional (test approvals).
- Exit 0 only if all pass; output diagnostics.

Update CLI to include `nib config edit`, `nib doctor`.

## Alternatives Considered

- Use only environment variables: Rejected — less structured for complex schemas like compression/memory.
- External config service: Overkill for local agent.

## Risks and Tradeoffs

- Config drift across profiles (mitigation: validation in doctor and runtime init).

## Rollout Plan

1. Define schema in code.
2. Implement doctor checks.
3. Wire into runtime startup (fail fast if invalid).
4. Tests and docs.

## Validation and Acceptance Criteria

- Full schema documented and parsed.
- `nib doctor` exits 0 on healthy setup, non-zero with clear errors otherwise.
- Runtime respects config (e.g., compression triggers per threshold).
- Aligned with T002 spec.

## Open Questions

- Default values and migration from current placeholders?