# Ecosystem Integration: MCP, Skills, and AGENTS.md

nib is not a standalone island. It must deeply integrate with the existing agentic tooling in this workspace to be effective.

Core requirements:
- **MCP** (Model Context Protocol)
- **Skills** (SKILL.md system used by Grok, skm/registry, and similar tools)
- **AGENTS.md** (and CLAUDE.md, etc.) — project-specific agent instructions

These are not optional features. They are fundamental to how coding and workload work happens here.

## AGENTS.md Support

Every serious project in the workspace has an `AGENTS.md` (or `CLAUDE.md`) at the root.

**Behavior**:
- When nib activates a project or task, it discovers the nearest supported instruction
  file: `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`, or `AGENTS.local.md`.
- Discovery walks up from the project root (or task workdir) and falls back to
  `$HOME/AGENTS.md` only when no workspace instruction file is found.
- Project standards and technical documentation are loaded separately from bounded,
  fixed roots inside the active project; nib does not implicitly load a sibling
  workspace's central `AGENTS.md`.
- Inject the selected instruction content into planning, execution, and reconciliation
  context.
- Respect the rules inside them (e.g., "MUST read docs/tech/backend_python.md before editing", "never update AGENTS.md yourself").

**Implementation**:
- `src/context/agents.rs` discovers AGENTS.md, AGENTS.local.md, CLAUDE.md, and
  CLAUDE.local.md from the active workspace hierarchy.
- The agent loop injects the resolved instruction context before planning and each
  execution step.
- Tool policy directives are loaded by `ToolExecutor`; selected skill usage and MCP
  results are persisted, but instruction-file provenance is not a separate session
  record.

Failure to follow loaded AGENTS.md should be treated as a serious violation during self-review or reconciliation.

## Skills Support

nib should participate in the SKILL.md ecosystem (used by the Grok skill system and similar tools).

**Capabilities**:
- Discover skills from standard locations:
  - `~/.grok/skills/`
  - Standard skill directories in the ecosystem (e.g. those used by Grok and similar tools)
  - Local project skills (`<project>/.nib/skills/`)
  - Paths from `[skills]` and the selected profile
- Parse SKILL.md frontmatter (YAML) + body.
- Load instructions, references, templates, and executable scripts.
- Dynamically activate skills for the current workload item (e.g. "activate symphony-spec-writing for planning this task").
- Contribute its own capabilities as skills (so other agents can use nib via the skill system).

**nib as a skill**:
- nib should be publishable as a skill itself (with frontmatter) so other agents can delegate workload/planning tasks to it.

**Implementation**:
- `src/context/skills.rs` parses YAML frontmatter and bodies, discovers configured
  roots, matches tags, and derives policy rules and after-tool hooks.
- `src/skill_cmd.rs` lists local/global skills and installs or removes global skills.
- Skills can provide:
  - Additional system prompt sections
  - Specialized tools / behaviors
  - References that get injected into context

See `~/work/projects/registry/SKILL_STRUCTURE.md` for the canonical format.

## MCP Support

MCP (Model Context Protocol) is the standard way tools and context are provided to agents in this environment (used by the current Grok TUI, Claude Desktop, and similar tools).

**nib must**:

1. **Act as an MCP client**
   - Connect to MCP servers configured in the active project's `.nib/config.toml`
     (GitHub, Notion, Linear, filesystem, custom ones).
   - Expose their tools to nib's own reasoning/planning/execution loops.
   - External agent or Grok MCP configuration is not imported automatically.

2. **Act as an MCP server**
   - Expose nib's gated core tools plus `nib_run` and `nib_get_status`.
   - This allows Claude Code, Grok subagents, and similar tools to call into nib for workload ownership instead of duplicating todo/kanban logic.

**Implementation**:
- `src/integrations/mcp.rs` manages configured stdio child servers and namespaces
  discovered tools as `server::tool`.
- `src/integrations/mcp_server.rs` serves JSON-RPC/MCP over stdio.
- `.nib/config.toml` owns command, arguments, environment, working directory, and
  request timeout configuration.
- Advertised tool invocations pass through `ToolExecutor`. Starting an outbound MCP
  child command is a separate, user-configured trust boundary.
- HTTP/SSE transports and OAuth are not implemented in this release.

Example configuration:
```toml
[mcp.servers.github]
command = "/path/to/pinned-server"
args = ["--stdio"]
request_timeout_secs = 30
```

## Integration Principles

- **Leverage, don't duplicate** (core architecture principle).
  - Use existing MCP servers the user already trusts.
  - Load skills the user has already installed/curated.
  - Respect AGENTS.md instead of inventing new rules.
- **Context is king for nib**.
  - When starting work on a task, the first thing nib does internally is assemble rich context:
    1. The selected workspace instruction file
    2. Active skills
    3. Connected MCP tools
    4. Project libs documentation (as previously discussed)
    5. Current workload state
- **Permissions apply universally** (see the full deep dive in [docs/tech/permissions.md](permissions.md)).
  - Reading AGENTS.md / SKILL.md is low-risk (read-only, scoped).
  - MCP tool exposure and activation must respect the approval modes, path scoping, and command classification.
  - Destructive actions (especially via `run_terminal` or broad patches) must never bypass user approval or explicit policy, even when triggered through skills or MCP.
- **Workload awareness**.
  - Session records persist selected skill usage and MCP tool results. The selected
    instruction-file path and complete configured MCP-server inventory are not stored as
    separate provenance manifests.

## Current Boundaries

- Runtime "workload" references mean profile-scoped session JSON with `PlanStep`
  state plus durable daemon task records. They do not imply a global SQLite backlog.
- Outbound MCP is stdio-only and configured per project.
- The inbound MCP server is also stdio-only. HTTP/SSE transports and OAuth are future
  work rather than part of the v1 contract.
- Inbound MCP cannot use stdin for interactive approvals; uncovered risky calls fail
  closed unless an explicit policy or configured approval covers them.
- Remote skills are explicit CLI installations and are not signed; users must review
  their instructions, constraints, and hooks. Installation copies only the manifest
  and declared resources under count/depth/file/aggregate bounds; Git checkout is
  noninteractive and time-bounded.
- The gateway normalizes Console, Telegram, Slack, and Discord payloads and dispatches
  them through profile-scoped agent sessions. Provider authentication, webhook/socket
  listeners, and reply delivery belong to external adapters.
- Gateway callers cannot inject tool schemas. The agent loop exposes only the tools
  configured by nib and gated through `ToolExecutor`.
- Session records preserve their own active-skill usage and every MCP tool result,
  while daemon audit records cover scheduled delivery and maintenance.

These integrations make nib a participant in the user's existing agent stack while
keeping execution policy and persistence local.
