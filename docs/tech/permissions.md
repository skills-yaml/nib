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
4. **Workload Accountability** — Every agent-selected action is recorded against its session and persisted plan in nib's workload model.
5. **Isolation First** — Prefer executing changes in isolated environments (git worktrees) rather than the user's main checkout.
6. **Transparency** — The user should always be able to answer "what did the agent just do and why was it allowed?"
7. **Graduated Autonomy** — Start manual, move to smart/policy-based only after the user has built trust.

## Threat Model Boundary

nib treats repository contents, symlinks/reparse points, stale state generations,
concurrent nib processes, child processes, tool output, and external service responses as
untrusted. It uses retained handles, no-follow traversal, conditional publication,
worktrees, process scopes, approval, redaction, and audit to detect or contain those
boundaries.

A malicious peer process already running as the same operating-system user is outside
the supported isolation boundary. Such a peer can normally inspect or modify nib's
memory, executable, configuration, Git metadata, and state directories. In particular,
ordinary unprivileged Unix APIs cannot conditionally unlink a pathname by an already
opened inode after a peer replaces that pathname, and Git provides no cross-platform
switch that makes repository-local configuration immutable for one invocation. nib
therefore proves exact namespace detachment, validates managed Git configuration before
launch, and fails closed on observable identity changes, but does not claim security
against an adversarial same-UID replacement in the final syscall interval.

This exclusion does not weaken the process-containment contract for descendants that
nib starts. Those children remain untrusted and must be contained by the managed-process
backend. Deployments that require protection from a hostile same-UID peer need an
additional account, VM/container boundary, or privileged cleanup/config broker outside
the current product.

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
- Tool paths must remain under the selected profile workspace. Symlinks and canonical paths are checked before access.
- Project-file mutations through `apply_patch` and `run_terminal` execute in the session
  worktree. Runtime-state tools write only to their bounded `.nib` stores.
- Worktree mode is the default for code edits and build commands.

### Layer 2: Isolation
- Use `git worktree` for almost all implementation work (prevents polluting main branch, easy rollback, parallel agents).
- Consider additional isolation for very high-risk commands (e.g. temporary Docker containers, though this has UX cost).
- Background/cron tasks should run with even stricter scoping.

### Layer 3: Policy Engine
- Static + dynamic rules.
- Rules can come from:
  - Built-in safe/destructive command lists
  - Supported directives inside the workspace's AGENTS.md
  - Active Skills (a skill can say "this command is destructive in this context")
- Policy can say: "always block", "require approval", "allow if in worktree and tests will be run after".

### Layer 4: Approval Workflow
This is the most important layer for destructive actions.

**Modes** (inspired by common patterns in advanced agent runtimes, adapted for nib):
- `manual` (default): Every destructive action prompts the user via TUI or CLI. User must explicitly say "approve", "approve for this task", or "deny".
- `smart`: Use a small auxiliary model or rules engine to auto-approve low-risk variants of safe commands. Still prompt for anything classified destructive.
- `policy`: Only actions covered by an explicit loaded allow rule execute. Unmatched risky actions fail closed without opening an interactive prompt.
- `off` / yolo: Only for throwaway environments or when the user is actively supervising. Should be very visible (status bar warning, etc.).

**Explicit Permission Mechanisms**:
- In-the-moment approval through the CLI prompt or TUI modal.
- AGENTS.md policy directives using `nib-policy: allow|deny|require-approval <tool> [argument text]`.
- Active-skill `constraints` for deny/require-approval rules and audited post-tool hooks.
- `approvals.mode` and the explicit run-level `--yes` override.

AGENTS.md can also tighten the execution envelope:

- `nib-sandbox: require-bwrap` forces strict `bwrap` execution.
- `nib-boundary: disable-network` forces strict `bwrap`, the restricted profile, and a disabled network namespace.
- `nib-boundary: profile <name>` selects a configured
  `execution.boundary_profiles.<name>` overlay. Network policy may only become stricter
  and writable paths may only be removed; invalid, unknown, conflicting, or weaker
  selections deny execution before approval or worktree creation.

These directives are tighten-only; project instructions cannot weaken configured isolation.

**Approval UX**:
- CLI approval prints the tool, permission level, and complete arguments before accepting `y`.
- TUI approval shows the tool and arguments and accepts an explicit `Y` or `N` decision.
- The source, decision, arguments, timestamp, plan link, and execution result are recorded in the originating session.

### Layer 5: Output & Secret Control
- **Secret redaction** applies to tool outputs before they enter agent context and is
  always enabled. Configured credentials and sensitive environment values extend the
  generic credential-pattern matcher.
- The current redactor targets secrets; it is not a general-purpose PII scrubber.
- Environment variable names may be audited, but configured sensitive values are
  redacted from model context and persisted tool records.

### Layer 6: Audit & Reconciliation
- Every tool call is logged to the workload store with:
  - Bounded, redacted command/patch arguments
  - Classification
  - Approval decision + source (user click, policy, yolo)
  - Result (success/failure, stdout summary, files changed)
  - Worktree used
- The reconciler (and human) can later review the full history of tool usage for a session and plan.
- This creates accountability: "The agent did X because the user approved it at 14:32".

### Layer 7: AGENTS.md & Project Rules
- nib loads the nearest supported workspace instruction file, or `$HOME/AGENTS.md` as
  a fallback, through the context loader before tool-using steps.
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
2. **Worktree Requirement** — Project-file mutations through `apply_patch` and
   `run_terminal` execute inside the current session's dedicated worktree. Memory,
   scheduling, and other runtime-state tools use their bounded `.nib` stores instead.
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
```rust
pub struct ToolMetadata {
    pub name: String,
    pub permission_level: PermissionLevel,
    pub requires_approval: bool,
    pub requires_worktree: bool,
    pub mcp_exposable: bool,
    pub input_schema: serde_json::Value,
}
```

### Approval Manager
Separate component with pluggable strategies (manual, smart, policy).

### Configuration
- Project runtime: `.nib/config.toml` (`approvals.mode`, execution boundaries, and profiles)
- Per-project policy: supported directives in AGENTS.md and active skill constraints
- Runtime override via `nib run --yes ...` or the equivalent TUI/run configuration

### UI
- TUI should have excellent approval dialogs (rich diff, clear risk callout, quick actions).
- CLI should fall back to rich prompts.
- Status indicator when in elevated mode.

### Storage
Approvals, events, plans, and tool history live in profile-scoped session JSON files,
linked by session and plan IDs. Daemon decisions use a profile-scoped JSONL audit log.
This durable local record is critical for trust and debugging.

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
   b. There is a clear loaded, auditable policy from AGENTS.md, an active Skill, or configuration that covers this exact action.
5. The decision is recorded in the workload model.
6. Output is redacted.
7. Any loaded AGENTS.md rules are satisfied.

If any layer says "no", the action does not happen.

This multi-layer approach, combined with nib's workload ownership and explicit context loading (AGENTS + Skills + MCP), gives much stronger guarantees than a raw agent with a "yolo" mode.

---

**Related Documents**
- `docs/tech/ecosystem_integration.md`
- [FT-001: Basic Agent Tools](../specs/development/ft_001_basic_agent_tools.md)
- [T001: Core Agent Tools](../specs/done/T001_implement_core_agent_tools.md)
- Workspace references for permission patterns in advanced agent tools (approvals, yolo modes, worktree isolation, redaction, shell-hook allowlists)

This model should be the foundation for nib's `ToolExecutor` and all future tool/MCP/skill integrations.
