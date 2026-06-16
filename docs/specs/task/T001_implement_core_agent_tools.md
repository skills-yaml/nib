# T001: Implement Core Agent Tools (FT-001)

**Related Feature:** [FT-001: Basic Agent Tools Implementation](../feature/ft_001_basic_agent_tools.md)

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

**Owner:** To be assigned  
**Estimate:** 3–6 focused sessions (depending on how much of the executor vs. individual tools is done in one go)  
**Next:** Create detailed `.plan.md` using the subagent-driven-development process when ready to start coding.