# T002: Agent Framework Runtime and Orchestration Engine for nib

**Related:** FT-002 Base Architecture, FT-001 Basic Agent Tools, T001 Core Agent Tools, docs/tech/architecture.md, docs/tech/permissions.md, docs/tech/ecosystem_integration.md

## Summary

One-sentence description: Define and implement a robust Agent Framework Runtime and Orchestration Engine for nib, fully mapped out according to the symphony-spec-writing standard, including a detailed ASCII Sequence Diagram that shows exactly how interactions occur from end to end.

State the intended outcome: nib gains a context-efficient, dynamically extensible autonomous agent processing loop with cross-session persistence, modular skill extensions, and a robust multi-platform execution gateway, while maintaining its strengths in workload ownership, safe permissions, and project standards enforcement.

## Problem Statement

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

- Robust Multi-Platform Gateway: Deliver programmatic execution interfaces bridging multiple messaging layers (Telegram, Slack, Discord, Console) with unified tool schemas.

- Full Runtime and Orchestration: Map out the Agent Framework Runtime and Orchestration Engine per the symphony-spec-writing standard, with a detailed end-to-end ASCII Sequence Diagram.

- Alignment with nib strengths: Preserve workload model (backlog/working/done organization per updated SDLC), ToolExecutor permissions (defense-in-depth, approvals), worktree isolation, and leverage of existing ecosystem (MCP, skills, AGENTS.md).

### Non-Goals

- Inventing a new neural LLM architecture (relies entirely on standard client-server bindings to APIs like OpenRouter, DeepSeek, Anthropic, or OpenAI).

- Replacing standard OS package managers (the terminal tool leverages host package managers like apt, brew, and uv).

## System Overview

The agent runs as an autonomous agent processing loop on the host system. It comprises four primary components:

1. The CLI / Interface Gateway: Intercepts incoming messages, manages TUI inputs, or listens across various messaging channels.

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

## Gap Analysis Summary (for context)

Current nib provides strong foundations in workload management, ToolExecutor with permissions, partial context/skills/MCP, and architecture docs. However, it lacks the full runtime state machine, dynamic context compression, discrete memory stores, profiles, maintenance daemons, and complete end-to-end orchestration loop described above.

## Alignment Tasks in Backlog

The following tasks (created alongside this one) will close the gaps:

- T003: Implement Context Engine with Dynamic Compression and Session Management

- T004: Profiles, Discrete Memory Store, and Maintenance Daemons

- T005: Full Runtime State Machine and Lifecycle

- T006: Enhanced Skills Framework and MCP Gateway Alignment

- T007: Configuration Schema Alignment + Validation

- T008: End-to-End Tests and Sequence Diagram Validation

These tasks will evolve the current ToolExecutor and context systems into the full engine while preserving nib's workload and permission strengths.

This brings nib into full alignment with the target architecture while keeping its identity.