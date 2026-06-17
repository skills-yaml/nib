# T006: Enhanced Skills Framework and MCP Gateway Alignment

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

## Problem Statement

nib's current skills support (discovery in `skills/`, basic activation and instruction injection in context/loader.py) is limited to heuristic matching and prompt prepending. It lacks full runtime integration: skills do not dynamically inject procedural strategies into the system prompt based on task tags, nor provide executable wrappers/constraints during ToolExecutor calls. MCP support is stub-only (basic client/server in `integrations/mcp.py`) without robust gateway capabilities for unified tool schemas across platforms or seamless delegation.

This results in brittle extensibility — agents cannot easily adapt to custom local rules, remote APIs, or environment scripts without code changes — and fails to deliver the "discoverable framework" and "robust multi-platform gateway" from the target architecture. Users cannot leverage the full power of modular SKILL.md extensions or MCP for interoperability with other agents.

## Goals

- Enhance Skills Framework: Full support for SKILL.md (YAML frontmatter + Markdown body + assets) with runtime prompt injection on relevant task tags, discoverability, and executable integration (wrappers, post-hooks, constraints) into ToolExecutor and runtime loop.
- MCP Gateway Alignment: Production-ready client (consume external tools like GitHub/Notion with unified schemas) and server (expose nib's runtime/tools via MCP for other agents to delegate workload/safe execution).
- Dynamic Extensibility: Skills and MCP tools adapt to local (AGENTS.md) and remote contexts without hardcoding.
- Integration: With context engine (T003), state machine (T005), permissions (enforce on all exposed tools), workload (record MCP/skill usage), and profiles (T004).
- Support for the end-to-end sequence diagram in T002 (include steps for skill matching and MCP calls).

## Non-Goals

- Building new MCP servers from scratch (leverage protocol; focus on nib's side).
- Full asset execution in skills (start with prompt injection + Python hooks; expand later).
- Replacing existing MCP usage in the Grok TUI (nib enhances/complements it).

## Proposed Design

**Enhanced Skills (build on current `skills/`):**
- **Discovery**: Expand to include project-local, user-global, and registry sources (as currently sketched). Parse frontmatter fully (use pyyaml for structured tags, version, etc.).
- **Runtime Injection**: In Context Engine (T003) and runtime state machine (T005), match skills by task tags/description. Inject body + references into system prompt at BUILD_CONTEXT. Support "executable" sections (e.g., Python snippets or commands run via ToolExecutor).
- **Wrappers/Constraints**: Skills register hooks with ToolExecutor (e.g., pre-approval checks, post-execution verification, additional permission rules). E.g., a "safe-build" skill might wrap run_terminal to always follow with tests.
- **Curator Integration** (T004): Daemons manage skill lifecycle (usage tracking, stale cleanup, pinning).

**MCP Gateway:**
- **ClientManager**: Enhance to connect to configured servers (stdio/HTTP), list tools with unified schema (name, description, permission_level from nib's model), and call with permission enforcement (route through ToolExecutor).
- **Server**: Use `mcp` package to expose:
  - Core tools (via registry, with permission metadata).
  - Runtime capabilities (get_workload, get_context, execute_task).
  - Full loop entrypoints (for delegation).
- **Unified Schemas**: Tools from MCP and skills use consistent Pydantic models.
- **Gateway Features**: Support multi-platform by bridging (e.g., MCP as common interface); handle auth (OAuth per ecosystem patterns).
- **Permissions**: All inbound MCP calls must pass ToolExecutor approval layers (no bypass).

**Config** (T007):
```yaml
skills:
  paths:
    - ~/.config/nib/skills
    - ./skills
mcp:
  servers:
    github:
      type: stdio
      command: "mcp-github"
  client_enabled: true
  server_enabled: true
```

## Alternatives Considered

- Keep skills as static prompt only: Rejected — insufficient for "procedural strategies" and dynamic behavior.
- Pure MCP without skills: Rejected — skills provide the modular, file-based extensibility needed for local adaptation.
- Heavy framework (e.g., LangChain tools): Rejected (minimal deps; align to simple SKILL.md + MCP).

## Risks and Tradeoffs

- **Complexity**: Dynamic injection + wrappers add runtime overhead (mitigation: caching, optional per-skill).
- **Security**: Exposed MCP tools must not bypass permissions (design enforces this; audit all calls).
- **Compatibility**: Skills/MCP from ecosystem may need adaptation (provide migration helpers).

## Rollout Plan

1. **Phase 1**: Full SKILL.md parsing, tag-based injection in context, basic wrappers in ToolExecutor.
2. **Phase 2**: MCP client consumption (unified tools) and server exposure (registry + runtime).
3. **Phase 3**: Integration with T003/T005 (injection at BUILD_CONTEXT; calls in TOOL_EXECUTE), T004 daemons (skill curator), T007 config.
4. **Phase 4**: Tests (T008), diagram validation (T002), docs updates. Demo delegation via MCP.
5. Leverage existing symphony/subagent skills for implementation.

## Validation and Acceptance Criteria

- Skills inject dynamically and affect behavior (e.g., custom constraints in approvals).
- MCP client lists/calls external tools (with permissions applied).
- MCP server exposes nib tools/runtime (other agents can delegate safely).
- Full alignment with T002 sequence diagram (skill match and MCP steps present).
- Cross-session persistence of skill usage (via T004 memory).
- `task test` covers injection, wrappers, MCP flows; no permission bypasses.

## Open Questions

- How to version/pin skills in profiles (T004)?
- Exact MCP transport for multi-platform (stdio vs. SSE priority)?
- Conflict resolution when skills + AGENTS.md rules disagree on a tool?