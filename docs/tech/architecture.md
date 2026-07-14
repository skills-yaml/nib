# Base Architecture of nib

nib is a **local-first AI coding agent**. All session data (conversations + tool calls) is stored as JSON files inside the project's `.nib/sessions/` directory. It breaks down goals, executes work safely (using hybrid bwrap + worktrees), and keeps full history per project.

This document describes the **base architecture** — the core components, data flows, principles, and integration points that every part of nib must respect.

See also:
- [Project Structure](project_structure.md)
- [Backend Rust](backend_rust.md)
- [Permissions](permissions.md) (defense-in-depth model)
- [Ecosystem Integration](ecosystem_integration.md) (MCP, Skills, AGENTS.md)
- [FT-001: Basic Agent Tools](../specs/feature/ft_001_basic_agent_tools.md)
- [FT-003: Hybrid Sandboxing](../specs/development/ft_003_adopt_codex_sandboxing.md) (reopened — implement in Rust)
- [FT-004: LLM Integration and Agent Loop](../specs/feature/ft_004_llm_integration_and_agent_loop.md) (reasoning + tool loop)

## High-Level Principles

1. **Session History is Sacred**
   - All conversation messages and tool executions are stored as files inside the project's `.nib/sessions/` directory.
   - This gives full per-session auditability without a global database.
   - Every tool call records the exact arguments, result, approval decision, worktree, and boundaries.

2. **Defense-in-Depth for Safety**
   - No tool action (especially destructive ones like `run_terminal` or broad patches) can occur without passing multiple independent layers: scoping, isolation (worktrees), classification, policy/AGENTS.md rules, explicit approval (manual or prior grant), redaction, and audit.
   - See the full [Permissions](permissions.md) document.

3. **Leverage, Don't Duplicate**
   - Reuse existing ecosystem primitives: kanban/todo/delegation/cron patterns, subagent patterns, MCP servers (GitHub, Notion, etc.), Skills (SKILL.md), AGENTS.md guidelines.
   - nib's job is **orchestration + safe execution + session history**, not reimplementing a general-purpose agent.

4. **Fresh Context + Verification Loops**
   - Prefer fresh sub-agents, isolated worktrees, and clean context for implementation work.
   - Two-stage review (spec compliance then quality) and post-execution reconciliation are default.
   - The agent (and sub-agents) must load and respect relevant AGENTS.md + active Skills before acting.

5. **Human-in-the-Loop by Default**
   - Status, blockers, decisions, and risks are highly visible (CLI + TUI).
   - Escalation points (clarify, approve, review diff) are first-class.

6. **Context-Rich but Token-Efficient**
   - Early in a session: assemble rich context (AGENTS.md, relevant Skills, project standards/libs docs, recent session history, connected MCP tools).
   - Future work will address token budgets via summarization and selective loading.

## Core Components

```
User / Workload Owner
        │
        ▼
┌──────────────────────────────┐
│         CLI (Rust) / TUI     │  (clap / ratatui)
└──────────────┬───────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│              Context Loader                   │  (AGENTS.md walk-up, Skills discovery/activation,
│  (src/context/)                               │   project standards, libs docs, MCP tool list)
└──────────────┬────────────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│              Context + Planner              │
│  (AGENTS.md/skills + LLM reasoning)           │
│  src/context/ + src/agent/loop.rs             │
└──────────────┬────────────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│         Tool Executor (the Gatekeeper)        │  (src/tools/executor.rs)
│  • Tool Registry (metadata + PermissionLevel) │
│  • Scoping + Worktree isolation               │
│  • Classification (read-only / safe /         │
│    destructive / network)                     │
│  • Policy + AGENTS.md + Skills constraints    │
│  • Approval workflow (manual/smart/policy/off)│
│  • Redaction                                  │
│  • Dispatch to impls + Audit to SessionStore  │
└──────────────┬────────────────────────────────┘

               │
       ┌───────┴───────┬───────────────┬──────────────┐
       ▼               ▼               ▼              ▼
┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
│ Core Tools  │ │ Integrations│ │ MCP Client  │ │ Sub-agents  │
│ (read_file, │ │ (git,       │ │ / Server    │ │ (subagent   │
│  list_dir,  │ │  subagent,  │ │ (expose     │ │  profiles,  │
│  grep,      │ │  lanes,     │ │  nib tools  │ │  lanes,     │
│  apply_patch,│ │  github...) │ │  to others) │ │  etc.)      │
│  run_terminal)│ └─────────────┘ └─────────────┘ └─────────────┘
└─────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│              Reconciler                       │  (Verify diffs, run tests, update workload state,
│  (src/agent/reconciler.rs - future)           │   link artifacts, record rationale)
└──────────────┬────────────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────────┐
│           Session Store (file-based)         │  (plain JSON files in <project>/.nib/sessions/)
│  • Conversation history (messages)            │
│  • Tool calls with results and approvals      │
│  • Stored per project under .nib/             │
└───────────────────────────────────────────────┘
```

### Key Modules (current + planned)

- `src/session/` — Session models and Project-local SessionStore (files in .nib/sessions/)
- `src/llm/` — LlmClient trait + providers (OpenAI, Anthropic, Gemini, Grok, OpenRouter, Meta, Mock)
- `src/agent/` — Core loop (reasoning → tool selection via executor → observation)
- `src/tools/`
  - `models.rs` — PermissionLevel, ApprovalMode, ToolCall, Result
  - `registry.rs` — Static metadata registration
  - `executor.rs` — The central gate (all layers)
  - `core_tools.rs` — Implementations of minimal tools
  - `worktree.rs` — Git worktree isolation manager
- `src/context/`
  - `agents.rs` — Walk-up discovery + loading of AGENTS.md
  - `loader.rs` — assemble_context() + assemble_agent_prompt()
- `src/sandbox/`
  - `mod.rs` — Hybrid execution via bwrap
- `src/integrations/` — MCP ClientManager + server exposing tools
- `src/mcp_cmd.rs` & `src/skill_cmd.rs` — Dedicated CLI managers for MCP servers and Skills
- `src/cli/` & `src/tui/` & `src/chat.rs` — Interfaces for interacting with the agent loop (including interactive REPL `/mcp` and `/skills` slash commands)

## Data Flow for a Typical Session

1. **Intake / Activation**
   - User creates or resumes a session (`.nib/sessions/<id>.json`).
   - Context + prompt builder assembles AGENTS.md, skills, recent history, and tool schemas.

2. **LLM Reasoning (new in FT-004)**
   - AgentLoop sends prompt to LLMClient.
   - LLM returns content or tool_calls.

3. **Execution (gated)**
   - Tool calls go to ToolExecutor:
     - Scope + worktree resolution.
     - Classification + policy/AGENTS enforcement.
     - Approval gate.
     - Hybrid sandbox dispatch (bwrap + boundaries).
     - Full ToolCallRecord written to the session file.
   - Observation appended to session and fed back to LLM.

4. **Loop + Reconciliation**
   - Continue until final answer, approved plan, or limit.
   - Future Reconciler can append verification summaries to the session.

5. **Visibility**
   - CLI/TUI shows live session history, tool calls (with boundaries/approvals), and loop state.

### Sequence Diagram of Interactions

```mermaid
sequenceDiagram
    participant U as User
    participant C as CLI / Chat
    participant L as Agent Loop
    participant Cx as Context Engine
    participant M as MCP Manager
    participant LLM as LLM Provider
    participant T as Tool Executor
    participant S as Session Store

    U->>C: Provides goal or chat input
    C->>L: Start Agent Run (goal, session_id)
    
    rect rgb(30, 40, 50)
        Note over L,S: Turn Initialization
        L->>S: Load or create Session
        L->>M: Initialize MCP Client (Load external tools)
        L->>T: Register Core + MCP Tools
    end
    
    loop Until Goal Met or Max Steps
        L->>Cx: Assemble Context (AGENTS.md, Skills, History)
        Cx-->>L: System Prompt + Injected Skills
        
        L->>LLM: Send Messages + Tools Schema
        LLM-->>L: Response (Text + Tool Calls)
        
        L->>S: Append Assistant Response
        
        alt Has Tool Calls
            loop For each Tool Call
                L->>T: Execute Tool
                T->>T: Verify Permissions & Classify
                T->>T: Check Approval / Sandbox Policy
                T-->>L: Tool Result / Observation
                L->>S: Append Tool Observation
            end
        else No Tool Calls
            L->>L: Mark as Done
        end
    end
    
    L-->>C: Return Agent Run Summary
    C-->>U: Display Results / Prompt for Input
```

## Persistence

- File-based sessions stored locally in the project folder under `.nib/sessions/`.
- Each session is a JSON file containing conversation messages and tool call records (with approvals, worktree, etc.).
- No central global database; everything is project-local.
- Future: export/import, git-friendly snapshots, or optional bridges to Notion/GitHub Projects.

## Integration Points (Ecosystem)

- **AGENTS.md / CLAUDE.md**: Automatically discovered and injected. Rules can influence classification, require extra approvals, or define safe-mode allowlists.
- **Skills (SKILL.md)**: Discovered from standard locations in the ecosystem (e.g. `~/.config/nib/skills` and project-local `.nib/skills`). Configured via `nib skill` or `/skills`. Provide instructions, constraints, wrappers, or post-hooks. nib itself can be published as a skill.
- **MCP**: Client consumes external tools (GitHub, Notion, etc.). Configured via `nib mcp` or `/mcp`. Server exposes nib tools (workload queries + safe executor calls) so other agents can delegate work to nib's permission model.
- **Sub-agents / Lanes**: Delegation targets with fresh context + worktree. nib owns the lifecycle and reconciliation.
- **Git**: Worktree isolation for changes; status/diff helpers.
- **Libs Documentation**: Read-only scoped access during context assembly so the agent understands domain boundaries (see previous requirements).

## Technology & Quality

(See [Backend Rust](backend_rust.md) for full details.)
- Rust (edition 2021) is the exclusive runtime.
- `tokio` for async execution.
- `clap` for the CLI and `ratatui` for the TUI.
- `reqwest` and `rustls` for LLM and network interactions.
- `serde` for all JSON and TOML parsing.
- All repeatable work via Taskfile.

## Future Evolution

- Streaming support for LLM clients.
- Richer Planner (full symphony-style + multi-step reasoning).
- Advanced session memory (summaries, facts across turns).
- Smarter approval classifier inside the loop.
- Deep sub-agent / lane delegation with linked sessions.
- MCP server exposing the full agent loop.
- TUI live streaming of LLM tokens + tool results.

This architecture keeps nib as a trustworthy, local-first orchestrator that drives LLMs safely through gated tools while maintaining complete per-project session history in `.nib/sessions/`.

Update this document whenever core components, flows, or principles change.