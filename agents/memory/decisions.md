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

## 2026-06-20 - Rust CLI Rewrite + LLM/Agent Loop Merged (PR #1)

- Type: milestone / decision
- Source: user + implementation
- Confidence: high
- Supersedes: Python-only CLI (Typer)

Content:

Merged feat/implement-basic-agent-tools (PR #1) into main.

- Primary CLI is now the Rust binary (`nib` via clap).
- `nib chat` supports only `/model` for switching (list + number select or direct name for active provider).
- `nib auth` wizard for multi-provider configuration.
- Hybrid: Rust CLI + Python core (LLM via LiteLLM, agent loop, tools, sessions in `.nib/`).
- FT-003 (hybrid bwrap sandbox) and FT-004 (LLM + agent loop) completed and moved to done/.
- CI, release, and install scripts follow skm patterns.
- All execution still updates the authoritative session state in `.nib/sessions/`.

Branch deleted locally. Main is now at merge commit e47cb7f.

Note: FT-003 was later **reopened** (2026-07-02) — sandbox was never implemented despite this milestone text.

## 2026-07-02 - FT-005 Pure Rust Migration — scope locked

- Type: decision
- Source: user
- Confidence: high
- Review: none
- Supersedes: hybrid Python core as long-term architecture (transitional until FT-005 Phase 6)

Content:

Approved FT-005 move to `development/` with these scope decisions:

- **Config:** Migrate to `.nib/config.toml`; auto-migrate from legacy `config.json`.
- **LLM:** Full provider set day one in Rust — OpenAI, Anthropic, Google Gemini, Grok (xAI), OpenRouter, Mock (no LiteLLM, no phased provider rollout).
- **TUI:** Port to ratatui in FT-005 Phase 4 (`nib tui`); in scope, not deferred.
- **FT-003:** Reopen to `development/`; implement hybrid sandbox only in Rust (FT-005 Phase 5 / T019).

Next implementation unit: T009 (module layout + TOML config migration).
