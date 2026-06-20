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
- When nib activates a project or task, it **must** discover and load the relevant `AGENTS.md` files.
- Walk up from the project root (or task workdir) until it finds one.
- Also load the central reference: `~/work/projects/agents/AGENTS.md` and `docs/tech/*` when appropriate.
- Inject the content into planning, execution, and reconciliation context.
- Respect the rules inside them (e.g., "MUST read docs/tech/backend_python.md before editing", "never update AGENTS.md yourself").

**Implementation**:
- `nib/context/agents.py` or `core/context.py`: `load_agents_instructions(project_path: Path) -> str`
- Cache per-project.
- Expose via workload model (store which AGENTS.md were active for a task).
- The `load_project_standards` tool (see previous design) should prioritize these.

Failure to follow loaded AGENTS.md should be treated as a serious violation during self-review or reconciliation.

## Skills Support

nib should participate in the SKILL.md ecosystem (used by the Grok skill system and similar tools).

**Capabilities**:
- Discover skills from standard locations:
  - `~/.grok/skills/`
  - Standard skill directories in the ecosystem (e.g. those used by Grok and similar tools)
  - Local project skills (e.g. `<project>/.skills/` or `skills/`)
  - The central registry (`~/work/projects/registry/skills/`)
- Parse SKILL.md frontmatter (YAML) + body.
- Load instructions, references, templates, and executable scripts.
- Dynamically activate skills for the current workload item (e.g. "activate symphony-spec-writing for planning this task").
- Contribute its own capabilities as skills (so other agents can use nib via the skill system).

**nib as a skill**:
- nib should be publishable as a skill itself (with frontmatter) so other agents can delegate workload/planning tasks to it.

**Implementation**:
- `nib/skills/` package with:
  - `discovery.py`
  - `loader.py` (parses frontmatter using `pyyaml`)
  - `registry.py` (in-memory + persistent activation)
- Skills can provide:
  - Additional system prompt sections
  - Specialized tools / behaviors
  - References that get injected into context

See `~/work/projects/registry/SKILL_STRUCTURE.md` for the canonical format.

## MCP Support

MCP (Model Context Protocol) is the standard way tools and context are provided to agents in this environment (used by the current Grok TUI, Claude Desktop, and similar tools).

**nib must**:

1. **Act as an MCP client**
   - Connect to configured MCP servers (GitHub, Notion, Linear, filesystem, custom ones).
   - Expose their tools to nib's own reasoning/planning/execution loops.
   - Use the same MCP servers the user has already configured in the Grok session.

2. **Act as an MCP server** (highly recommended)
   - Expose nib's core capabilities as MCP tools:
     - Workload queries (`get_tasks`, `get_project_status`)
     - Planning (`create_plan`, `decompose_goal`)
     - Delegation helpers
     - Reconciliation status
   - This allows Claude Code, Grok subagents, and similar tools to call into nib for workload ownership instead of duplicating todo/kanban logic.

**Implementation**:
- Add `mcp` package to dependencies.
- `nib/integrations/mcp.py`:
  - `MCPClientManager` — connect to servers from config or env.
  - `MCPServer` — serve nib tools.
- Configuration: simple TOML/JSON or reuse existing `~/.grok/` or project-local config.
- All MCP tool calls must go through nib's permission/approval layer (see permissions design).
- Support stdio and HTTP/SSE transports.

Example config sketch (later):
```toml
[mcp.servers]
github = { command = "...", args = [...] }   # or url for remote
notion = { ... }
```

## Integration Principles

- **Leverage, don't duplicate** (core architecture principle).
  - Use existing MCP servers the user already trusts.
  - Load skills the user has already installed/curated.
  - Respect AGENTS.md instead of inventing new rules.
- **Context is king for nib**.
  - When starting work on a task, the first thing nib does internally is assemble rich context:
    1. Relevant AGENTS.md files
    2. Active skills
    3. Connected MCP tools
    4. Project libs documentation (as previously discussed)
    5. Current workload state
- **Permissions apply universally** (see the full deep dive in [docs/tech/permissions.md](../permissions.md)).
  - Reading AGENTS.md / SKILL.md is low-risk (read-only, scoped).
  - MCP tool exposure and activation must respect the approval modes, path scoping, and command classification.
  - Destructive actions (especially via `run_terminal` or broad patches) must never bypass user approval or explicit policy, even when triggered through skills or MCP.
- **Workload awareness**.
  - Store in the workload model which AGENTS.md, skills, and MCP servers were active for each task. This creates an auditable history of *how* work was done.

## Next Steps for Implementation

1. Create `src/nib/context/` for AGENTS.md + project standards loading.
2. Create `src/nib/skills/` for discovery and loading.
3. Add MCP client/server in `src/nib/integrations/mcp.py`.
4. Wire them into the planner and executor so they are automatically used.
5. Add configuration support (e.g. `nib config mcp add ...`).
6. Expose "load context" as a first-class internal step (visible in TUI).

This makes nib a first-class participant in the user's existing agent stack instead of yet another isolated tool.
