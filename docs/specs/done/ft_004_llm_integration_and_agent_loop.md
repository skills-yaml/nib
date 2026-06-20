# FT-004: LLM Integration and Agent Loop

**Status:** Done (merged 2026-06-20 via PR #1)  
**Related:** [FT-001: Basic Agent Tools](../feature/ft_001_basic_agent_tools.md), [FT-002: Base Architecture](../feature/ft_002_base_architecture.md), [FT-003: Hybrid Sandboxing](../feature/ft_003_adopt_codex_sandboxing.md), [docs/tech/architecture.md](../../tech/architecture.md), [docs/tech/ecosystem_integration.md](../../tech/ecosystem_integration.md)

## Summary

Add first-class support for driving nib via LLMs, including a pluggable LLM client, structured prompt assembly from sessions + context, a core agent loop (reason → tool selection → execution via ToolExecutor → observation), and tight integration with the new per-project `.nib/sessions/` persistence for conversation history and tool call records.

This enables nib to act as a full autonomous coding agent while preserving all safety, audit, and human-steerability guarantees.

## Problem Statement

nib currently has:
- Excellent low-level execution (ToolExecutor + hybrid bwrap sandbox + worktrees + permissions).
- Session-based persistence (conversations + tool calls stored in project-local `.nib/sessions/*.json`).
- Context loading (AGENTS.md, skills, MCP tools).
- No LLM driving the reasoning or agentic loop.

Without an LLM + loop:
- Users can only use tools manually via `demo-tool` or future CLI commands.
- No autonomous decomposition, planning, or multi-step execution.
- Cannot leverage the "chief of staff + senior engineer" vision described in the product foundation.

The loop must:
- Use the sacred session history as context.
- Route every action through the gated ToolExecutor.
- Support Plan Mode, approvals, and fresh sub-agent delegation.
- Remain local-first and leverage the existing Grok/MCP/skills ecosystem.

## Goals

- Provide a pluggable `LLMClient` abstraction supporting OpenRouter, OpenAI, Anthropic, Google Gemini, Grok (via LiteLLM), plus a Mock for development.
- Build a core `AgentLoop` that:
  - Loads/resumes sessions from `.nib/sessions/`.
  - Assembles rich prompts (AGENTS.md + skills + session history + tool schemas + current workload snapshot).
  - Drives ReAct / tool-calling style reasoning.
  - Executes tools exclusively via `ToolExecutor` (so sandbox, approvals, and recording are automatic).
  - Appends all LLM turns and tool results back to the session file.
- Support "Plan Mode" (read-only exploration + structured plan output before any execution).
- Enable human-in-the-loop at natural points (approval gates, clarification requests).
- Make the loop usable from CLI, TUI, MCP, and as a sub-agent target.
- Keep token usage efficient and respect AGENTS.md rules.

## Non-goals (for this feature)

- Building a full custom LLM from scratch.
- Replacing existing sub-agent ecosystems (Grok lanes, MCP, skills) — nib orchestrates them.
- Complex long-term memory beyond sessions + context snapshots (future FT).
- Web UI or multi-user concerns.
- Full support for every possible LLM feature (vision, etc.) in v1.

## Proposed Design

### 1. LLM Client Abstraction

```python
# src/nib/llm/base.py
from typing import Protocol, Any

class LLMClient(Protocol):
    async def complete(
        self,
        messages: list[dict],
        tools: list[dict] | None = None,
        **kwargs
    ) -> LLMResponse:
        ...

class LLMResponse(BaseModel):
    content: str | None = None
    tool_calls: list[ToolCallRequest] | None = None
    finish_reason: str
    usage: dict | None = None
```

Implementations (via LiteLLM for unified support):
- OpenRouter
- OpenAI
- Anthropic (Claude)
- Google (Gemini)
- Grok (xAI)
- MockLLMClient for testing.

Configuration via `.nib/config.toml` or env (model, api_key, base_url).

### 2. Prompt Construction

Extend `context/loader.py` → `assemble_agent_prompt(session: Session, goal: str, mode: str) -> str`

Components:
- Static system rules from AGENTS.md + loaded skills.
- Current session history (messages + summarized tool_calls).
- Available tools (from `ToolRegistry`, with permission metadata).
- Project context (via safe read tools or snapshot).
- Current mode (plan vs execute).

Tool schemas are auto-generated from `ToolMetadata`.

### 3. Core Agent Loop

```python
# src/nib/agent/loop.py
async def run_agent_loop(
    session_store: SessionStore,
    session_id: str,
    goal: str,
    llm: LLMClient,
    executor: ToolExecutor,
    max_steps: int = 30,
    mode: str = "execute",  # "plan" | "execute"
) -> Session:
    session = session_store.load_session(session_id) or ...
    session_store.append_message(session_id, "user", goal)

    for step in range(max_steps):
        prompt = assemble_agent_prompt(session, goal, mode)
        response = await llm.complete(...)

        if response.tool_calls:
            for tc in response.tool_calls:
                # Always goes through the gate
                result = await executor.execute(
                    ToolCall(tool_name=tc.name, arguments=tc.arguments, session_id=session_id),
                    session_id=session_id
                )
                session_store.append_message(session_id, "tool", str(result))
                # Record detailed ToolCallRecord already happens inside executor
        else:
            session_store.append_message(session_id, "assistant", response.content)
            if mode == "plan" or is_final_answer(response):
                break

    return session
```

The loop lives in `src/nib/agent/loop.py`.

### 4. Integration with Existing Components

- **SessionStore** (`.nib/sessions/`): Primary memory for the loop. Every LLM message and tool result is appended.
- **ToolExecutor**: Single point of execution. The loop never calls tools directly.
- **ContextLoader**: Supplies AGENTS.md, skills, and (future) lightweight project snapshot.
- **Worktree + Hybrid Sandbox** (FT-003): Automatically applied on any mutating tool call.
- **MCP / Sub-agents**: The loop itself can be exposed. Can delegate steps to fresh Grok sub-agents (recording their output into the parent session).
- **CLI/TUI**: New commands `nib run "goal"` and interactive mode. TUI shows live session + loop steps.

### 5. Plan Mode

When `mode="plan"`:
- Only read-only tools are allowed in the executor for that step (or a temporary restricted executor).
- LLM is instructed to output a structured plan (following symphony style).
- Plan is written to the session.
- User must explicitly approve before switching to execute mode.

This mirrors the Grok-style safety we want.

### 6. Updates to Models & Persistence

- Extend `SessionMessage` if needed for `tool_calls` references.
- `ToolCallRecord` already captures everything the loop needs.
- No changes to the file-based storage format (JSON per session).

### 7. Updates to Core Components

- `src/nib/llm/` – new package.
- `src/nib/agent/` – loop + prompt builder.
- `src/nib/core/workload.py` (SessionStore) – minor additions (e.g., `get_recent_history(n)`).
- `src/nib/context/loader.py` – `assemble_agent_prompt(...)`.
- `src/nib/cli/app.py` – add `run` command.
- Architecture diagram updated to include LLMClient + AgentLoop between Context and Executor.

## Alternatives Considered

| Approach | Pros | Cons | Decision |
|----------|------|------|----------|
| ReAct only (no native tool calling) | Simple, works with any LLM | Verbose prompts, harder parsing | Use native when available + fallback |
| Global LLM session memory | Easy cross-project | Violates new per-project `.nib/` rule | Rejected |
| Full LangGraph / CrewAI | Rich loops | Heavy deps, duplicates our safety layers | Rejected (leverage instead) |
| Pure prompt chaining (no loop) | Simple | No multi-step autonomy | Insufficient for vision |
| Make the loop itself an MCP tool | Great for delegation | Circular if nib calls itself | Supported as secondary |

## Rollout Plan

1. **Phase 1**: LLMClient + basic AgentLoop (single turn + tool execution). Update demo to use the loop.
2. **Phase 2**: Full multi-step loop + session history feeding. Wire Plan Mode.
3. **Phase 3**: CLI `nib run`, TUI live view, MCP exposure.
4. **Phase 4**: Grok-native client + sub-agent delegation tools + token budgeting.
5. Update FT-002/FT-003 and architecture.md.

Backward compatible: old direct tool use still works; the loop is additive.

## Validation and Acceptance Criteria

- [ ] `nib run "fix the bug in foo.py"` creates a session, runs the loop, records all messages + tool calls in `.nib/sessions/`.
- [ ] Every tool action goes through `ToolExecutor` (sandbox, approval, audit).
- [ ] Plan Mode produces a structured plan with no side effects until approved.
- [ ] Session history is used for context in subsequent turns (demonstrated by multi-step task).
- [ ] AGENTS.md and skills influence tool selection and behavior.
- [ ] `task check` and `task test` pass (add loop tests).
- [ ] End-to-end demo in the repo root succeeds and leaves clean session artifact.
- [ ] Architecture.md and this spec are updated and cross-referenced.

## Open Questions

- Exact prompt template format (system + history + tools + scratchpad).
- How much of the prior tool output to include vs summarize in history.
- Streaming support for TUI (LLM tokens + tool output live).
- Cost / token limits per session (future config).
- Support for vision / multi-modal in tool results.
- Whether the loop should auto-spawn sub-agents for complex sub-goals.

---

**Next steps after this spec**: Implement Phase 1 (LLM client + minimal loop) using the new session persistence. Prioritize Grok client and clean integration with `ToolExecutor`.

Update this spec and `agents/memory/decisions.md` as decisions are made during implementation.