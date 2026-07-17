# T006: Enhanced Skills Framework and MCP Gateway Alignment

**Status:** Development

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

> Historical proposal note: the Python hooks, HTTP/OAuth ideas, and MCP-stub language
> below describe the pre-Rust baseline. The 2026-07-15 reconciliation defines the
> shipped SKILL.md framework and stdio-only MCP v1 boundary.

## Historical Problem Statement (Proposal-Time)

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

## Historical Proposed Design

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

## Reopened Audit (2026-07-15)

Scope: add structured SKILL.md metadata and configured discovery, executable
constraints/hooks with usage audit, and permission-preserving MCP client/server flows.

Affected areas: `src/context/skills.rs`, `src/tools/`, `src/integrations/`,
`src/config/`, `src/session/`, and skills/MCP tests.

Validation gates: injection/constraint/usage tests, MCP client/server no-bypass tests,
`task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Ship bounded SKILL.md discovery/parsing/injection, references/assets, policy and
after-tool hooks, plus permission-preserving outbound and inbound MCP over stdio.

### Transport And Adapter Boundary

The historical HTTP/SSE MCP and OAuth design is future work. Shipped v1 MCP transport
is configured stdio. Telegram, Slack, and Discord authentication/listeners/replies are
external adapters; nib owns only the normalized gateway dispatch contract.

### Acceptance Criteria

- [x] Structured skills are discovered from configured/profile roots and injected by relevance or explicit activation.
- [x] Skill policy rules and after-tool hooks pass through normal executor approval and audit.
- [x] Outbound MCP tools are namespaced, schema-bounded, called through `ToolExecutor`, and audited.
- [x] The inbound stdio MCP server exposes gated core/runtime tools.
- [x] Selected skill usage is persisted in each originating session.
- [x] Cross-session skill-usage aggregation is proven as a durable curator input.
- [x] Fresh local repository gates are green on the reconciled tree.
- [ ] Windows runtime gates are green on the reconciled tree.

### Affected Areas

`src/context/skills.rs`, `src/skill_cmd.rs`, `src/integrations/mcp.rs`,
`src/integrations/mcp_server.rs`, `src/integrations/gateway.rs`, `src/tools/`,
`src/session/`, `src/daemons/curator.rs`, and tests.

### Implementation Evidence

- `src/context/skills.rs` parses manifests/references/assets and derives policy/hooks.
- `src/integrations/mcp.rs` and `src/integrations/mcp_server.rs` implement stdio client/server boundaries.
- `src/integrations/gateway.rs` implements normalized, tool-schema-closed dispatch.
- `src/session/mod.rs` keeps skill-use records authoritative and serializes usage writes
  with session deletion and curator reads, rejecting names that cannot map to a bounded
  canonical skill ID. `src/daemons/curator.rs` rebuilds a bounded profile-wide aggregate
  for each cleanup, enforcing a total session-byte budget before parsing, and derives
  skill retention from the latest persisted use; no secondary usage index can diverge
  after a partial write.

### Validation Evidence

- `src/context/skills.rs`: structured parse, policy, symlink, count, and byte-bound tests.
- `tests/test_runtime_e2e.rs`: selected-skill denial and permission-gated MCP delegation.
- `src/integrations/mcp.rs`: timeout, cancellation, environment, schema, and child-lifecycle tests.
- `src/integrations/mcp_server.rs`: initialize/list/call/status/error/no-audit-bypass tests.
- `src/daemons/curator.rs`:
  `cross_session_skill_usage_survives_restart_and_drives_retention`,
  `concurrent_skill_usage_updates_are_not_lost_from_the_aggregate`,
  `legacy_active_skill_without_usage_timestamp_is_retained_fail_closed`,
  `raw_skill_name_uses_the_installer_slug_for_managed_retention`,
  `aggregate_session_byte_budget_blocks_reads_and_managed_skill_deletion`, and
  corrupt/symlink fail-closed cleanup tests. The shared canonical identifier maps
  installation and curator usage names to the same bounded directory slug.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
remediation gates below are authoritative for completion.

- [x] Deterministic skill and stdio MCP client/server security tests exist.
- [x] Cross-session skill-usage/curator evidence.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

HTTP/SSE MCP, OAuth, and in-process provider chat transports are not implemented and
are not claimed as v1. The gateway lock remediation below supersedes this earlier
assessment of the remaining in-scope work.

## Final Quality Review Remediation (2026-07-15)

### Scope

Derive persisted gateway session identifiers from the exact adapter platform and
conversation identity without lossy collisions, and serialize concurrent deliveries
that target the same persisted session. Preserve a second persistent identity anchor
for each session lock so replacing one visible lock path cannot create an overlapping
lock domain, and make every retry cause honor cancellation, deadlines, and polling.

### Acceptance Criteria

- [x] Distinct external conversation identities cannot map to the same persisted nib session.
- [x] IDs remain path-safe and bounded while retaining a readable prefix.
- [x] Collision regressions cover punctuation differences and shared long prefixes.
- [x] Concurrent deliveries for one platform/conversation cannot run overlapping agent
  loops or overwrite/interleave the same authoritative session state.
- [x] A deterministic same-conversation concurrency regression proves serialized runs.
- [x] A held same-session lock remains authoritative if its primary path is renamed or
  replaced; a contender must block or fail closed against the persistent anchor.
- [x] An actual child-process regression proves lock visibility and replacement defense
  across process boundaries rather than only between handles in one process.
- [x] Interrupted lock attempts check the same absolute deadline as contention and sleep
  between retries, preventing timeout bypass and CPU spin.

### Affected Areas

`src/integrations/gateway.rs`, dependency metadata if required, gateway tests, and the
Windows test job in `.github/workflows/ci.yml` where it exercises the cross-platform
locking implementation.

### Validation Gates

Focused gateway lock identity, child-process replacement, cancellation, and interrupted
retry tests; `task test`, `task check`, `task coverage`, and Windows CI `task test`.

### Validation Evidence

Gateway identifier collision and concurrent dispatch regressions cover the existing
session mapping and serialization contract. The process-level replacement regression
holds the persistent profile-parent anchor while replacing both the primary lock file
and the complete `sessions/` directory. The interrupted retry regression proves the
absolute timeout is retained and polling sleeps between attempts. Focused local tests
pass. Fresh local Task gates, 83.94 percent coverage (53,734/64,015), the locked build,
and Linux
release/PTY smoke passed on 2026-07-16; Windows CI execution remains an outstanding
runtime validation gate.

## Remaining Implementation Plan

1. Execute the configured Windows gateway lock, cancellation, replacement, and full
   runtime suites.
2. Remediate any Windows lock-identity or retry behavior while preserving the persistent
   anchor, absolute deadline, and polling guarantees.
3. Rerun the canonical Task gates and two-stage review before moving T006 to `done/`.

## Current Risks

- The process-visible gateway lock and replacement defense have not been executed on
  Windows.
- A platform-specific lock change could split the authoritative lock domain or weaken
  cancellation/deadline behavior unless the existing process regressions remain mandatory.
