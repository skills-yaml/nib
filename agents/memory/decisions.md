# Decisions

## 2026-06-17 - Adopt workspace-docs@1.0.0

- Type: decision
- Source: user
- Confidence: high
- Review: none
- Supersedes: none

Content:

nib adopts the workspace documentation standard at `workspace-docs@1.0.0`. Adoption is additive: preserve project-specific guidance and legacy specs, and keep generated agent context inside `AGENT-CONTEXT` markers.

## 2026-06-19 - LLM Integration and Agent Loop (FT-004)

- Type: decision
- Source: user + planning
- Confidence: high
- Review: none
- Supersedes: previous global workload model (replaced by per-project .nib/sessions/)

Content:

Adopt FT-004 for LLM-driven agent loop. Key decisions:
- Sessions (conversations + tool calls) are the primary memory, persisted as JSON files in <project>/.nib/sessions/.
- No central global projects/tasks in the runtime persistence layer.
- Pluggable LLMClient (Grok-first).
- AgentLoop routes every action through ToolExecutor (hybrid bwrap + worktrees + boundaries + plan gates).
- Rich context from AGENTS.md + skills + session history.
- Plan Mode support before execution.
- Full audit trail in session files.
- Leverage existing ecosystem (MCP, subagents, skills) instead of duplicating.

This completes the shift from workload-centric to session + execution-centric architecture while preserving safety and human steerability.
