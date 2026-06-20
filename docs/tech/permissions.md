# Permission Management for Agents (Deep Dive for nib)

This document provides a comprehensive model for managing permissions in AI coding agents, with a strong focus on preventing destructive actions without explicit user approval or clear policy consent.

It is informed by:
- Real production patterns from dominant agent runtimes in this workspace (such as those providing rich approval and isolation features)
- Workspace conventions (AGENTS.md files, subagent-driven-development, worktree usage)
- General security best practices for agentic systems (least privilege, defense in depth, auditability)
- nib's unique role as a **workload-owning orchestrator** (not just another chat agent)

## Core Philosophy

We do **not** want an agent that can "do whatever it wants."  
nib should be powerful enough to be useful for real coding work, but conservative enough that users can trust it with their codebase and machine.

Key principles:
1. **Least Privilege by Default** — The agent starts with almost no power.
2. **Defense in Depth** — Multiple independent layers must all agree before a dangerous action happens.
3. **Explicit Consent** — Destructive or high-impact actions require either (a) direct user approval in the moment or (b) an explicit, auditable policy the user has previously granted.
4. **Workload Accountability** — Every action (especially approved ones) is recorded against a specific Task in nib's workload model.
5. **Isolation First** — Prefer executing changes in isolated environments (git worktrees) rather than the user's main checkout.
6. **Transparency** — The user should always be able to answer "what did the agent just do and why was it allowed?"
7. **Graduated Autonomy** — Start manual, move to smart/policy-based only after the user has built trust.

## Action Classification

Every tool call or action must be classified into one of these levels:

| Level          | Examples                                      | Default Approval | Can be auto-approved? |
|----------------|-----------------------------------------------|------------------|-----------------------|
| **Read-only**  | read_file, list_directory, grep, git_status   | Never            | Yes (always)         |
| **Safe**       | apply_patch (clean, in worktree), write new file in allowed dir, git add | Policy-based     | Yes (with limits)    |
| **Destructive**| rm, git reset --hard, force push, global npm install, dropping tables | **Always manual or explicit policy** | Only with very strong explicit grant |
| **Network/Exfil** | curl, wget, sending data to external services, cloning private repos | Strict manual or policy | Rarely |

**Classification sources** (in priority order):
1. Static rules (hardcoded + config)
2. AGENTS.md project rules (e.g. "never run `cargo install` without approval")
3. Skill-provided constraints
4. Heuristic / small classifier (for `run_terminal` command parsing)

## Defense-in-Depth Permission Layers

No single layer is sufficient. Dangerous actions must pass **all** applicable layers.

### Layer 1: Scoping (Hard Boundaries)
- Agent is restricted to the current **project root** by default.
- Explicit allowlist for cross-project reads (e.g. shared `libs/` documentation or central `agents/docs/tech/`).
- Never allow writes outside the active project/worktree unless the Task explicitly requires a multi-project change **and** the user has granted a scoped exception.
- Worktree mode is the default for any edit or build work.

### Layer 2: Isolation
- Use `git worktree` for almost all implementation work (prevents polluting main branch, easy rollback, parallel agents).
- Consider additional isolation for very high-risk commands (e.g. temporary Docker containers, though this has UX cost).
- Background/cron tasks should run with even stricter scoping.

### Layer 3: Policy Engine
- Static + dynamic rules.
- Rules can come from:
  - Built-in safe/destructive command lists
  - `~/.nib/policy.toml` (user global)
  - `<project>/.nib/policy.toml` or rules inside AGENTS.md
  - Active Skills (a skill can say "this command is destructive in this context")
- Policy can say: "always block", "require approval", "allow if in worktree and tests will be run after".

### Layer 4: Approval Workflow
This is the most important layer for destructive actions.

**Modes** (inspired by common patterns in advanced agent runtimes, adapted for nib):
- `manual` (default): Every destructive action prompts the user via TUI or CLI. User must explicitly say "approve", "approve for this task", or "deny".
- `smart`: Use a small auxiliary model or rules engine to auto-approve low-risk variants of safe commands. Still prompt for anything classified destructive.
- `policy`: Only actions that have an explicit prior grant (from policy files, AGENTS.md allowlists, or `nib allow ...` commands) are auto-approved. Everything else blocks or prompts.
- `off` / yolo: Only for throwaway environments or when the user is actively supervising. Should be very visible (status bar warning, etc.).

**Explicit User Permission Mechanisms**:
- In-the-moment approval (TUI dialog with diff preview, command preview, and "why" explanation).
- Task-scoped grants: "Allow destructive commands for this Task only".
- Global / project-scoped grants via `nib permissions allow "run_terminal:install" --project . --scope task`
- Allowlist in AGENTS.md (e.g. a special section that nib parses):
  ```markdown
  ## Allowed Destructive Commands
  - "cargo install" when inside a worktree and followed by tests
  ```
- One-time "nib allow" commands that write to a small local allowlist.

**Approval UX Requirements**:
- Always show the **full command** or **exact patch**.
- Show the **reason** the agent wants to do it (from the plan or current step).
- Show **risk level** and **what could go wrong**.
- Offer "Approve", "Approve for this task", "Deny + explain", "Switch to worktree first".
- Record the decision, who approved (user or policy), and timestamp in the workload store.

### Layer 5: Output & Secret Control
- **Secret redaction** on all tool outputs (terminal stdout, file reads, etc.) before they enter agent context. Configurable but on by default for safety.
- PII redaction where relevant.
- No raw environment variables or credential files should ever be returned to the model unless explicitly allowed for a specific task.

### Layer 6: Audit & Reconciliation
- Every tool call is logged to the workload store with:
  - Full command/patch
  - Classification
  - Approval decision + source (user click, policy, yolo)
  - Result (success/failure, stdout summary, files changed)
  - Worktree used
- The reconciler (and human) can later review the full history of tool usage for a Task.
- This creates accountability: "The agent did X because the user approved it at 14:32".

### Layer 7: AGENTS.md & Project Rules
- nib **must** load the project's AGENTS.md (and central references) via the context loader before any tool-using step.
- Rules in AGENTS.md can override or add to the permission policy (e.g. "Treat `git push --force` as always destructive even in worktree").
- The agent is explicitly instructed in its system prompt to follow loaded guidelines.

### Layer 8: Skills as Constraint Providers
- Skills can register:
  - Additional classification rules
  - Wrappers around tools (e.g. a "safe-build" skill that only allows certain build commands)
  - Post-action hooks (after patch, must run tests)
- Activation of a skill can temporarily tighten permissions.

### Layer 9: MCP Boundaries
When tools are exposed via MCP (so other agents can call nib):
- The permission layers still apply on nib's side.
- The calling agent does **not** get to bypass approvals.
- Calls coming over MCP should be treated with equal or higher scrutiny.
- nib can surface "this tool call requires user approval" back to the caller.

## Special Handling for Destructive Actions

Focus on `run_terminal` and broad file changes:

1. **Command Parsing** — Maintain a growing list of known dangerous patterns (`rm -rf`, `git reset --hard`, `DROP DATABASE`, `sudo`, `curl ... | sh`, force pushes, etc.). Use regex + simple AST for shell commands where possible.
2. **Worktree Requirement** — For any command that modifies source, prefer (or require) execution inside a dedicated worktree for the current Task.
3. **Preview + Confirmation** — Before approval, show a dry-run or diff where possible.
4. **Chained Actions** — If the agent wants to do several destructive things, require approval for the whole plan, or force step-by-step.
5. **Post-Action Verification** — After a destructive or build action, strongly encourage (or require) running verification (tests, lint) as part of the same step.

## Implementation Recommendations for nib

### Central ToolExecutor
All tool usage must go through a single `ToolExecutor` class.

Responsibilities:
- Resolve current scope (project + worktree + allowlists)
- Load current context (AGENTS.md + active skills)
- Classify the action
- Consult policy engine
- Trigger approval UI if needed
- Execute (or delegate to integration)
- Redact output
- Record everything to workload store with approval metadata

### Tool Metadata
Each tool should declare:
```python
class ToolMetadata(BaseModel):
    name: str
    permission_level: Literal["read_only", "safe", "destructive", "network"]
    requires_approval: bool = True
    requires_worktree: bool = False
    mcp_exposable: bool = True
    # ...
```

### Approval Manager
Separate component with pluggable strategies (manual, smart, policy).

### Configuration
- Global: `~/.nib/config.toml` (approvals.mode, default redaction, etc.)
- Per-project: `.nib/policy.toml` or rules in AGENTS.md
- Runtime overrides via CLI flags (`nib --yolo ...` should be very loud)

### UI
- TUI should have excellent approval dialogs (rich diff, clear risk callout, quick actions).
- CLI should fall back to rich prompts.
- Status indicator when in elevated mode.

### Storage
Approvals and tool history live in the SQLite workload store, linked to Tasks. This is critical for trust and debugging.

## Recommended Defaults for nib

- `approvals.mode = "manual"` (conservative)
- Worktree mode on by default for coding tasks
- Secret redaction on
- Strong preference for `apply_patch` over raw writes
- All destructive `run_terminal` calls start blocked until user or explicit policy allows
- Clear visual distinction in TUI when the agent is about to do something risky

## Common Pitfalls to Avoid

- Letting the LLM decide its own permission level ("this is safe, trust me").
- Approvals that are too coarse ("approve all for the next hour").
- Forgetting that sub-agents / delegated tasks also need the same controls.
- Exposing full tool power over MCP without the permission layers traveling with the call.
- Silent auto-approvals that the user later regrets because they weren't obvious in the UI.

## Summary: How to Block Destructive Actions

To perform a destructive action, the following must all be true:

1. The action is in scope for the current project/worktree.
2. The action is isolated (preferably in a worktree).
3. The action is classified as destructive (or higher).
4. Either:
   a. The user explicitly approves in the moment, **or**
   b. There is a clear, previously granted, auditable policy (from AGENTS.md, config, or `nib allow`) that covers this exact action in this context.
5. The decision is recorded in the workload model.
6. Output is redacted.
7. Any loaded AGENTS.md rules are satisfied.

If any layer says "no", the action does not happen.

This multi-layer approach, combined with nib's workload ownership and explicit context loading (AGENTS + Skills + MCP), gives much stronger guarantees than a raw agent with a "yolo" mode.

---

**Related Documents**
- `docs/tech/ecosystem_integration.md`
- `docs/specs/feature/ft_001_basic_agent_tools.md`
- `docs/specs/task/T001_implement_core_agent_tools.md`
- Workspace references for permission patterns in advanced agent tools (approvals, yolo modes, worktree isolation, redaction, shell-hook allowlists)

This model should be the foundation for nib's `ToolExecutor` and all future tool/MCP/skill integrations.