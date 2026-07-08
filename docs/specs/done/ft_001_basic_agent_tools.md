# FT-001: Basic Agent Tools Implementation

**Status:** In Progress (core implemented; write tools stubbed)  
**Owner:** nib team  
**Related:** [Product Foundation](../foundation/product.md), [Ecosystem Integration](../../tech/ecosystem_integration.md), [Permissions & Safety](../../tech/permissions.md), [Base Architecture](../../tech/architecture.md)

## Overview

nib requires a small, well-defined set of core tools to function as an effective coding and workload agent. These tools enable the agent to inspect, modify, and execute work in a local development environment while maintaining strict safety, permission, and audit boundaries.

This feature defines the minimal viable tool surface, their interfaces, permission classifications, and integration points with the rest of the nib system (workload model, planner, executor, reconciler, MCP, Skills, and AGENTS.md).

The design prioritizes safety and leverage over breadth: reuse existing patterns from subagent-driven-development and the workspace conventions rather than reinventing a large tool library.

## Goals

- Provide exactly the minimal set of tools needed for high-quality coding work (read, search, edit, execute) without over-permissioning.
- Enforce a consistent, auditable permission model across all tools.
- Make every tool invocation go through the workload model (record what was done, why, and the outcome).
- Ensure tools are usable both directly by nib's core and exposable via MCP so other agents (Grok, Claude, and similar) can delegate to nib.
- Support dynamic behavior via Skills (e.g., a skill can contribute additional tool constraints or post-processing).
- Automatically respect any `AGENTS.md` / project guidelines loaded for the current context (e.g., "never run `npm install` without approval").
- Enable safe parallel execution via worktree isolation for edit/execute tools.
- Keep the surface small enough that it can be fully documented, tested, and reasoned about by both humans and sub-agents.

## Non-goals (for v1)

- Full browser automation or web interaction tools.
- Arbitrary database or cloud resource access (use MCP servers for that when needed).
- General-purpose code execution sandbox (rely on the host's normal shell + worktrees + approval). See FT-003 for direct bwrap sandboxing (with Codex patterns as reference).
- Semantic code search / embeddings (start with simple grep + read).
- Tool discovery or self-modification by the agent at runtime (static tool registry).
- Cross-project write access by default (read access to shared libs/docs is allowed via scoped context loading).

## Core Tools (Minimal Set)

All tools follow a common pattern:
- Take explicit `project_root` or `cwd` (enforces scoping).
- Return structured output (JSON-serializable Pydantic models where possible).
- Log every call to the workload store with outcome, duration, and any approvals granted.
- Respect the current context's loaded AGENTS.md rules.

### 1. `read_file`
- **Purpose**: Read file contents, optionally with line range.
- **Parameters**:
  - `path`: relative or absolute (must resolve inside allowed scope)
  - `start_line`, `end_line`: optional (0-based or 1-based, consistent with conventions)
- **Permission Level**: Read-only
- **Safety**: Path scoping + secret redaction on output (configurable).
- **Output**: `{"path": "...", "content": "...", "start_line": N, "end_line": M, "truncated": bool}`

### 2. `list_directory`
- **Purpose**: List files and directories.
- **Parameters**:
  - `path`
  - `recursive`: bool (default false, with sensible depth limit)
  - `include_hidden`: bool
- **Permission Level**: Read-only
- **Output (target)**: List of entries with type (file/dir), size, mtime.  
  Current basic impl returns only `path` + `type` (with recursive safety cap).

### 3. `grep` / `search_files`
- **Purpose**: Search file contents or filenames.
- **Parameters**:
  - `pattern`: regex or literal
  - `path`: scope (defaults to project root)
  - `glob`: e.g. `**/*.py`
  - `max_results`: int
- **Permission Level**: Read-only
- **Output (target)**: List of matches with file, line, snippet (redacted).  
  Current basic impl performs case-insensitive substring match (no true regex, no glob filter). Full ripgrep-style via terminal is planned.

### 4. `apply_patch` (preferred edit tool)
- **Purpose**: Apply a unified diff / patch safely.
- **Parameters**:
  - `path` or `worktree_id`
  - `patch`: string (unified diff)
  - `dry_run`: bool (default true for review)
- **Permission Level**: Safe write (or Destructive if it touches protected paths)
- **Safety**:
  - Must apply cleanly or fail explicitly.
  - Prefer execution inside an isolated git worktree.
  - After apply, optionally run a verification command.
- **Output**: `{"applied": bool, "hunks": [...], "conflicts": [...], "new_state": "diff"}`

### 5. `run_terminal`
- **Purpose**: Execute shell commands (the highest-risk tool).
- **Parameters**:
  - `command`: string
  - `cwd`: optional (defaults to current project/worktree)
  - `timeout`: seconds
  - `background`: bool (for long-running)
  - `worktree_id`: to force isolation
- **Permission Level**: Classified at call time (Read-only / Safe / Destructive / Network)
- **Safety** (mandatory):
  - Command classification before execution.
  - Approval workflow based on current `approvals.mode` (manual / smart / off).
  - Worktree isolation for any write or build command when possible.
  - Output streaming + redaction.
  - Hard timeout and kill handling.
  - Never execute from project root by default if a clean worktree is available for the task.
- **Output**: `{"exit_code": int, "stdout": "...", "stderr": "...", "duration": float, "approval_granted": bool}`

### Optional but recommended in v1
- `git_status`, `git_diff` (thin wrappers or restricted `run_terminal` aliases)
- `verify` (runs the project's canonical test/lint command and parses results)

## Permission & Safety Model

(See the full deep-dive document: [docs/tech/permissions.md](../../tech/permissions.md).)

Key points for tool implementation:
- All tools go through a central `ToolExecutor`.
- Classification into read-only / safe / destructive / network.
- Multiple enforcement layers (scoping, isolation/worktrees, policy, approval workflow, redaction, audit, AGENTS.md rules, skills constraints).
- Destructive actions require either real-time user approval **or** explicit prior permission (via policy, `nib allow`, or AGENTS.md allowlists).
- Every call (and its approval decision) must be recorded against the owning Task in the workload model.

Tools are classified into levels:
- Read-only
- Safe write (new files, clean patches in worktree)
- Destructive / High-risk (delete, force git, global installs, network writes)
- Network (outbound)

Enforcement layers (all must pass):
1. Path scoping (hard allowlist: current project + explicitly approved shared docs/libs roots).
2. Command / action classification.
3. Approval mode (manual prompt via TUI/CLI, smart classifier, or yolo).
4. Worktree isolation (default for edit/execute on coding tasks).
5. Audit + reconciliation (every tool call is recorded against the owning Task in the workload model).
6. AGENTS.md rules (e.g., "run `cargo test` only after `cargo check` succeeds"; "never commit directly").

Skills can add extra constraints or post-processing for specific tool calls.

MCP-exposed versions of these tools must carry the same permission metadata.

## Integration Points

### With Workload Model
- Every tool call is linked to the active Task/Project.
- Store: command/patch, approval decision, result summary, artifacts (diffs, logs).
- The reconciler uses tool history when deciding if a task is complete.

### With MCP
- All five core tools (plus verify) must be exposable as MCP tools.
- nib can act as both MCP client (consume external tools) and server (offer its tools + workload context to other agents).
- Tool calls arriving via MCP still go through the full permission pipeline.

### With Skills
- Skills can be activated per-task and influence:
  - Additional system instructions for tool use.
  - Custom tool wrappers (e.g., a "safe-rust-build" skill that wraps `run_terminal`).
  - Post-execution hooks (e.g., "after apply_patch, always run the project's test command").
- Discovery and activation happens via the context loader (already scaffolded).

### With AGENTS.md / Project Guidelines
- Context loader always injects the relevant AGENTS.md before any planning or tool-using step.
- The planner and any sub-agents must be explicitly told to follow the loaded guidelines.
- Tool usage that would violate loaded AGENTS.md should be blocked or escalated.

### With Libs Documentation (previous requirement)
- The context assembly step can safely load relevant shared libs docs and models (read-only, scoped) so the agent understands domain boundaries before using edit/execute tools.

## Implementation Approach (Python)

- Tool registry in `src/nib/tools/` (or `core/tools.py` initially).
- Each tool is a Pydantic-validated function + metadata (name, description, permission_level, requires_approval, mcp_exposable).
- Central `ToolExecutor` that:
  - Resolves current scope + active AGENTS.md + skills.
  - Classifies the call.
  - Obtains approval (via TUI dialog or smart policy).
  - Executes (prefer worktree for writes).
  - Records to workload store.
  - Returns structured result (with redaction applied).
- Use `asyncio` + `subprocess` (with PTY support for interactive feel where needed) for terminal.
- For patching: use `patch` utility or Python's `difflib` + validation; always prefer git apply inside a worktree.
- MCP layer in `integrations/mcp.py` wraps the same tool functions.
- Skills integration: skills can register additional constraints or wrapper functions at activation time.

Start with pure function-based tools (easy to test). Move to class-based registry only when dynamic skill contribution is needed.

## Implementation Status (as of 2026-06, feat/implement-basic-agent-tools)

The core permission model, registry, executor, worktree support, workload audit recording, context/AGENTS/skills loading, and MCP stubs were delivered as part of this feature. See [docs/tech/architecture.md](../../tech/architecture.md) for the as-built module summary.

**Delivered:**
- Tool models (PermissionLevel, Approval*, ToolCall/Result, ToolExecutionRecord)
- Registry + 5 tools registered with metadata
- ToolExecutor with scoping, worktree auto-selection for SAFE/DESTRUCTIVE, approval modes (MANUAL default with rich CLI prompt), basic redaction placeholder, audit recording to WorkloadStore
- WorktreeManager (git worktree create/cleanup/status)
- Functional read-only tools: `read_file`, `list_directory`, `grep` (basic Python impl; grep is substring case-insensitive)
- `apply_patch` and `run_terminal`: stub implementations (return preview messages; real `git apply` in worktree + asyncio subprocess + classification/redaction/TODOs remain)
- WorkloadStore: tables + `record_tool_execution` + history query (snapshot stubbed)
- Context assembly + `nib context` command exercising AGENTS.md + skills
- `nib demo-tool` exercising the executor + permission flow + DB
- CLI surface updated for tools demo

**Gaps vs this spec (tracked for follow-up):**
- Real implementation of edit/execute tools (apply_patch using git apply; run_terminal using subprocess or Codex sandbox per FT-003)
- Full dynamic classification for run_terminal; POLICY/SMART approval modes beyond fallback
- Secret redaction, improved output formats (size/mtime on list, regex on grep)
- TUI approval flows
- Rich E2E demo performing an actual code change + reconciliation inside worktree
- `task check` (format + 90 pyright errors in executor types) and expanded tests
- Deeper Skills/MCP/AGENTS influence inside the executor decision path

The skeleton and permission layers closely match the design in this spec and the finalized architecture.md.

## Acceptance Criteria

These remain the definition of full completion for FT-001. See the Implementation Status section above for delivered vs remaining work.

- [ ] All five core tools implemented with the interfaces above.
- [ ] Every tool call is recorded in the workload model with full audit trail.
- [ ] Path scoping + worktree isolation works for edit/execute tools.
- [ ] Approval workflow (manual + smart mode) is functional via both CLI and TUI.
- [ ] Tools are registered and callable both directly and via MCP (stub server is acceptable for v1).
- [ ] Relevant skills can influence tool behavior (at minimum, their instructions are injected; at least one skill provides a wrapper or constraint).
- [ ] Loaded AGENTS.md rules are visible to the planner and can block or require approval for specific tool uses (demonstrated with at least one rule from the nib project's own AGENTS.md or a test rule).
- [ ] All tool usage respects read-only access to libs documentation / project standards when context-loaded.
- [ ] Unit + integration tests exist for each tool (including permission denial paths and worktree scenarios).
- [ ] `task check` and `task test` pass.
- [ ] End-to-end demo: nib is given a small coding task, loads context (AGENTS + skills), uses the tools safely inside a worktree, records everything, and produces a verifiable result.

**Partial progress note**: The executor + recording + read tools + basic context + demo tooling satisfy several of the integration and audit requirements at the skeleton level. Full write-tool implementations, richer tests, and quality gates are the remaining work to close this feature.

## Open Questions / Risks

- Exact surface for the patch format (unified diff only? Support for "edit by search/replace" as well?).
- How sophisticated the "smart" approval classifier should be in v1 (simple rules + optional small LLM call?).
- Token budget for injecting full AGENTS.md + multiple skills into every planning step (need summarization or selective loading strategy).
- Whether `run_terminal` should support a "safe mode" that only allows commands from an allowlist defined in the project's AGENTS.md.

---

**Next after this spec**: Break into implementation tasks (T-XXX) with .plan.md files using the subagent-driven-development pattern. Prioritize the tool registry + permission executor + worktree helpers, then MCP exposure, then skill/MCP/AGENTS integration points.