# T002: Agent Framework Runtime and Orchestration Engine for nib

**Status:** Done

**Related:** FT-002 Base Architecture, FT-001 Basic Agent Tools, T001 Core Agent Tools, docs/tech/architecture.md, docs/tech/permissions.md, docs/tech/ecosystem_integration.md

> Historical proposal note: the problem, proposed SQLite/YAML design, and rollout
> sections below describe the pre-Rust baseline. The 2026-07-15 reconciliation is the
> authoritative profile-scoped JSON/TOML runtime design.

## Summary

One-sentence description: Define and implement a robust Agent Framework Runtime and Orchestration Engine for nib, fully mapped out according to the symphony-spec-writing standard, including a detailed ASCII Sequence Diagram that shows exactly how interactions occur from end to end.

State the intended outcome: nib gains a context-efficient, dynamically extensible autonomous agent processing loop with cross-session persistence, modular skill extensions, and normalized gateway dispatch for console and externally adapted messaging platforms, while maintaining its strengths in workload ownership, safe permissions, and project standards enforcement.

## Historical Problem Statement (Proposal-Time)

Complex autonomous developer agents frequently hit performance ceilings and context limits due to:

1. Inefficient Context Management: Bloating contexts with raw, unprocessed tool returns, standard library outputs, or massive terminal dumps, resulting in high LLM costs and decreased reasoning accuracy.

2. Brittle/Static Capabilities: Traditional agents lack the ability to adapt to a user's local directory rules, remote API interfaces, or environment-specific custom scripts without hardcoding code updates.

3. Fragile State Preservation: Failing to persist cross-session behaviors, preferences, and lessons learned leads to repetitive user steering and poor user confidence.

Current nib documentation (architecture.md, ecosystem_integration.md, permissions.md, FT-001/FT-002, T001) and implementation (ToolExecutor with basic permissions/worktree, context loaders, MCP stubs, workload model) provide a strong foundation for safe tools and workload, but fall short of a full-featured runtime and orchestration engine as described in the provided specification.

## Goals and Non-Goals

### Goals

- Context Preservation: Compress context dynamically once target thresholds are reached while retaining crucial session context.

- Dynamic Extensibility: Enable modular skill extensions (SKILL.md) structured through a discoverable framework.

- Cross-Session Persistence: Support durable SQLite-backed session histories and discrete, factual memory stores.

- Robust Multi-Platform Gateway: Normalize Console, Telegram, Slack, and Discord message payloads and dispatch them through the persisted agent loop using runtime-owned tool schemas. Telegram, Slack, and Discord transport adapters remain external to nib.

- Full Runtime and Orchestration: Map out the Agent Framework Runtime and Orchestration Engine per the symphony-spec-writing standard, with a detailed end-to-end ASCII Sequence Diagram.

- Alignment with nib strengths: Preserve workload model (backlog/working/done organization per updated SDLC), ToolExecutor permissions (defense-in-depth, approvals), worktree isolation, and leverage of existing ecosystem (MCP, skills, AGENTS.md).

### Non-Goals

- Inventing a new neural LLM architecture (relies entirely on standard client-server bindings to APIs like OpenRouter, DeepSeek, Anthropic, or OpenAI).

- Replacing standard OS package managers (the terminal tool leverages host package managers like apt, brew, and uv).

- Implementing Telegram, Slack, or Discord authentication, webhook/socket listeners, or reply delivery. External adapters own those transport concerns and pass authenticated payloads into nib's normalized gateway contract.

## System Overview

The agent runs as an autonomous agent processing loop on the host system. It comprises four primary components:

1. The CLI / Interface Gateway: Accepts console/TUI inputs and payloads received by external messaging adapters, normalizes them, and dispatches them through the regular persisted agent loop. External adapters authenticate provider traffic, run listeners, and deliver rendered replies.

2. Context Engine: Compiles host facts, merges loaded profiles, matches skills, maps tools, and manages chat contexts.

3. Execution Sandbox & Tool Dispatcher: Dispatches approved calls safely across File, Terminal, Code Execution, and Browser modules.

4. Maintenance Daemons (Cron & Curator): Manages offline recurring jobs and processes, and cleans up old memory.

## Core Domain Model

- Profile: Identifies a targeted runtime workspace, housing its own custom environment settings (.env), active databases, custom skills, and localized context databases.

- Session: Represented as an indexed string sequence of alternating user, assistant, and tool messages.

- Skill: A standalone package format (YAML frontmatter + Markdown body + referencing assets) that injects procedural strategies directly into the system prompt when relevant task tags trigger it.

- Memory Store: A discrete JSON key-value store segmenting environment configurations (memory) and user identity records (user).

## Configuration Schema

All instances MUST honor properties declared in the project's configuration (e.g. ~/.nib/config.yaml or equivalent). Essential structures are formatted as follows:

```yaml
model:
  default: "anthropic/claude-sonnet-4"
  provider: "openrouter"
  context_length: 200000

agent:
  max_turns: 90
  tool_use_enforcement: true

terminal:
  backend: "local"      # Options: local | docker | ssh | modal
  timeout: 180

compression:
  enabled: true
  threshold: 0.50       # Compress when current usage is 50% of context limit
  target_ratio: 0.20    # Compress down to 20% size

memory:
  memory_enabled: true
  provider: "built-in"
```

## Lifecycle and State Machine

The execution turn is managed as a finite state loop bounded by resource constraints (max_turns).

```
┌──────────────┐     UserInput     ┌────────────────────┐
│              │ ─────────────────>│                    │
│     IDLE     │                   │   BUILD_CONTEXT    │
│              │ <─────────────────│                    │
└──────────────┘    Final Text     └─────────┬──────────┘
       ▲                                     │
       │                                     │ System Prompt Ready
       │                                     ▼
       │                            ┌────────────────────┐
       │     Update Memory          │                    │
       ├─────────────────────────── │     INSPECT_LLM    │
       │                            │                    │
       │                            └────────┬──────────┘
       │                                     │
       │ Tool Results                        │ Generates Tool Schema Call
       │                                     ▼
┌──────┴───────┐                    ┌────────────────────┐
│              │                    │                    │
│ TOOL_EXECUTE │ <───────────────── │   USER_APPROVAL    │
│              │  Manual / Smart    │                    │
└──────────────┘                    └────────────────────┘
```

## Algorithms and Invariants

**Alternating Role Invariant**

The session conversation stream MUST strictly enforce message role alternation. Consecutive arrays of the same role are structurally forbidden and MUST be squashed, combined, or parsed programmatically prior to endpoint delivery. Formats strictly follow:

User -> Assistant (requests tools) -> Tool (returns outcomes) -> Assistant (resolves text) -> User

**Context Compression Trigger Pattern**

When sliding context length registers past threshold bounds:

- System elements are preserved.

- Old conversational logs are sent to an auxiliary LLM along with instructions to "summarize historic facts and code progress".

- Historic logs are cut, replacing them with a synthesized compact narrative message, reclaiming up to 80% free context buffer.

## Error Handling & Recovery

- Tool Execution Failures: If a subprocess exits non-zero, raw stderr MUST be delivered to the agent model to foster self-correction, rather than hiding failures or program crashes.

- API Network Timeouts: Leverages exponential backoff retries of the payload transport layer.

- Model Key Exhaustion: If multiple provider API keys exist configured in the key rotation pool, seamlessly re-routes traffic to active nodes if a 429 quota exhaustion is detected.

## Validation and Acceptance Criteria

1. System Introspection: Equivalent of "doctor" execution MUST pass with code 0 on active deployments.

2. Tool-Use Integration: Submitting complex programming tasks must execute real system changes, compile test suites, or return physical HTTP payloads rather than writing dummy stubs.

3. Gateway Dispatch: A deterministic mock-provider test MUST normalize a platform payload, dispatch it through the regular agent loop, and prove that repeated messages for one conversation reuse a valid profile-scoped session. Invalid normalized inputs MUST fail before session persistence, and gateway callers MUST NOT supply or override runtime tool schemas.

## End-to-End Sequence Diagram

The diagram below reflects a complete user-to-agent processing cycle, explaining how the framework routes messages, resolves matches, requests permissions, and modifies directory states.

```
User (TUI/Gateway)         Engine (CLI)            LLM Endpoint              Terminal/File Tool

      │                         │                              │                            │
      │   1. /slash or prompt   │                              │                            │
      │────────────────────────>│                              │                            │
      │                         │─┐                            │                            │
      │                         │ │ 2. Get CWD & profile facts │                            │
      │                         │<┘                            │                            │
      │                         │─┐                            │                            │
      │                         │ │ 3. Match relevant skills   │                            │
      │                         │<┘ (e.g. symphony-spec, devops)                            │
      │                         │                              │                            │
      │                         │─┐                            │                            │
      │                         │ │ 4. Build System Prompt &   │                            │
      │                         │<┘ Tool Schemes               │                            │
      │                         │                              │                            │
      │                         │ 5. POST payload (History)    │                            │
      │                         │─────────────────────────────>│                            │
      │                         │                              │─┐                          │
      │                         │                              │ │ 6. Model reasons &       │
      │                         │                              │ │    decides to call a tool│
      │                         │                              │<┘                          │
      │                         │ 7. Return Tool Payload JSON  │                            │
      │                         │<─────────────────────────────│                            │
      │                         │                              │                            │
      │                         │─┐                            │                            │
      │                         │ │ 8. Guard Check! Is command │                            │
      │                         │<┘    safe? (YOLO/Smart/Manual)                            │
      │                         │                              │                            │
      │                         │ 9. Execute raw system action │                            │
      │                         │──────────────────────────────────────────────────────────>│
      │                         │                              │                            │─┐
      │                         │                              │                            │ │ 10. Modifies files/
      │                         │                              │                            │ │     runs compiler
      │                         │                              │                            │<┘
      │                         │ 11. Return raw stderr/stdout                               │
      │                         │<──────────────────────────────────────────────────────────│
      │                         │                              │                            │
      │                         │ 12. Append to chat context   │                            │
      │                         │─┐                            │                            │
      │                         │ │ 13. [Optional] Context     │                            │
      │                         │<┘     Compression Trigger    │                            │
      │                         │                              │                            │
      │                         │ 14. POST context to endpoint │                            │
      │                         │─────────────────────────────>│                            │
      │                         │                              │─┐                          │
      │                         │                              │ │ 15. Synthesize final     │
      │                         │                              │ │     factual response     │
      │                         │                              │<┘                          │
      │                         │ 16. Return Text Response     │                            │
      │                         │<─────────────────────────────│                            │
      │                         │                              │                            │
      │   17. Render result     │                              │                            │
      │<────────────────────────│                              │                            │
      │                         │                              │ 
```

## Historical Gap Analysis (Proposal-Time)

At proposal time, nib provided workload management, basic ToolExecutor permissions,
and partial context/skills/MCP foundations, but lacked the complete engine. The later
reconciliation and linked tests record delivery of those capabilities.

## Historical Rollout Tasks (Completed)

The following tasks were created alongside this one and closed the proposal-time gaps:

- T003: Implement Context Engine with Dynamic Compression and Session Management

- T004: Profiles, Discrete Memory Store, and Maintenance Daemons

- T005: Full Runtime State Machine and Lifecycle

- T006: Enhanced Skills Framework and MCP Gateway Alignment

- T007: Configuration Schema Alignment + Validation

- T008: End-to-End Tests and Sequence Diagram Validation

These tasks evolved ToolExecutor and context systems into the shipped engine while
preserving nib's workload and permission boundaries.

This brings nib into full alignment with the target architecture while keeping its identity.

## Reopened Audit (2026-07-15)

Scope: complete the runtime/configuration/recovery/gateway requirements delegated to
T003-T008 and prove a real mutation/verification cycle.

Affected areas: `src/agent/`, `src/config/`, `src/context/`, `src/daemons/`,
`src/integrations/`, `src/session/`, `src/tools/`, and runtime E2E tests.

Validation gates: healthy `nib doctor`, diagram trace assertions, a physical
edit/build flow, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

The current engine is a bounded Rust state machine over profile-scoped JSON sessions,
structured plans, discrete JSON memory, durable background-task records, bounded
runtime context, gated tools, stdio MCP, and a normalized external-adapter gateway.

### Superseded Design Decisions

- The proposed SQLite/global backlog model is historical. `src/session/mod.rs` stores
  indexed sessions and `PlanStep` state under the selected profile; durable terminal
  and schedule work lives in `src/daemons/workload.rs`.
- Telegram, Slack, and Discord authentication, listeners, and reply delivery stay
  outside nib. `src/integrations/gateway.rs` accepts normalized, authenticated payloads.

### Acceptance Criteria

- [x] Bounded state transitions, plan approval, tool execution, and reconciliation are persisted.
- [x] Context combines AGENTS, selected skills, memory, workload, session history, and tools within `context_length`.
- [x] Profile sessions, memory, cron/curator state, and durable tasks survive restart.
- [x] Gateway payloads normalize and reuse path-safe profile sessions without caller-supplied tools.
- [x] Real worktree edits, MCP calls, compression, denial, and reconciliation are exercised.
- [x] Final coverage and repository gates meet the project thresholds.

### Affected Areas

`src/agent/`, `src/context/`, `src/session/`, `src/profile/`, `src/daemons/`,
`src/tools/`, `src/integrations/`, runtime entrypoints, and E2E tests.

### Implementation Evidence

- `src/agent/state.rs` and `src/agent/loop.rs` implement the lifecycle and bounds.
- `src/session/mod.rs`, `src/session/memory.rs`, and `src/daemons/workload.rs` implement
  the authoritative persistence model; `src/integrations/gateway.rs` implements the
  normalized boundary for externally hosted chat adapters.

### Validation Evidence

- `tests/test_runtime_e2e.rs`:
  `runtime_sequence_selects_profile_context_and_skill_then_reconciles_audited_tools`.
- `src/integrations/gateway.rs`:
  `dispatch_reuses_a_persisted_mock_agent_session` and
  `rejects_invalid_normalized_payloads_and_caller_supplied_tools`.
- `tests/durable_tasks.rs`: detached terminal and scheduled-wake process tests.

### Validation Gates

- [x] Deterministic lifecycle, gateway, persistence, and physical-edit tests exist.
- [x] `task coverage` reaches the documented runtime threshold.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

SQLite migration and in-process provider chat adapters are not gaps; they are
explicitly superseded or outside the normalized gateway contract. No in-scope runtime
gap remains.

## Reopened Audit (2026-07-16)

### Scope

Bind persisted plans and tool audit records to the exact current goal/plan revision,
and prove the documented edit-and-build lifecycle through the complete agent loop.

### Acceptance Criteria

- [x] A persisted plan has a stable unique ID and the exact goal it was generated for.
- [x] A new goal invalidates any incomplete or approved plan for another goal before
  planning, approval, worktree creation, or tool dispatch.
- [x] Tool audit records reference the exact active plan revision.
- [x] A deterministic full-loop test performs an approved physical edit, runs a real
  compile/test command in the same worktree, triggers compression, and reconciles the
  authoritative session.

### Affected Areas

`src/session/`, `src/agent/`, `src/tools/executor.rs`, Mock LLM scenarios, and runtime
E2E tests.

### Implementation Plan

1. Add generated plan identity and normalized goal fields with legacy-compatible
   deserialization.
2. Invalidate non-resumable plans before the state machine leaves `Idle`, and preserve
   same-goal incomplete plans.
3. Persist the active plan ID through approval and tool audit.
4. Exercise edit, real verification, compression, and reconciliation in one loop run.

### Risks

Existing legacy plans cannot be trusted for execution because they lack a goal binding;
they are invalidated with an audit event and regenerated. Exact goal matching is
deliberately case-sensitive after whitespace normalization to avoid accidental reuse.

### Completion Evidence

`Plan` persists a generated ID and normalized goal, Idle atomically invalidates
non-resumable plans, and a dedicated per-session run lease prevents concurrent loops
from sharing plan/worktree ownership. Loop regressions cover same-goal resume,
different-goal replanning, concurrent-run rejection, and stale approval CAS.
`full_agent_loop_compresses_edits_and_runs_real_cargo_tests_in_one_worktree` proves the
complete edit, test, audit, and reconciliation path.

### Validation Gates

Goal-reuse regression tests, full-loop edit/build/compression E2E, `task check`,
`task test`, and `task coverage`.
