# Memory Changelog

## 2026-06-17 - Initialize Agent Memory

- Type: fact
- Source: user
- Confidence: high
- Review: none
- Supersedes: none

Content:

Initialized `agents/memory/` for nib during additive adoption of `workspace-docs@1.0.0`.

## 2026-06-20 - PR #1 Merged: Rust CLI + LLM Agent Loop

- Type: release / milestone
- Source: merge
- Confidence: high

Content:

Pull request #1 (feat/implement-basic-agent-tools) merged into main.

Major deliverables:
- Full Rust CLI port (auth, chat with /model only, run, etc.).
- LLM integration (multi-provider via LiteLLM) + core agent loop.
- Per-project .nib/ sessions and config.
- Hybrid sandbox foundations and specs FT-003 / FT-004 marked done.
- skm-style CI, installers, and Task integration.

See decisions.md for details. Merge commit: e47cb7f.

## 2026-07-02 - FT-005: Pure Rust core implemented

- Type: implementation / milestone
- Source: FT-005 Phases 0–6
- Confidence: high

Content:

- Migrated agent loop, ToolExecutor, all 5 core tools, LLM providers (OpenAI, Anthropic, Gemini, Grok, OpenRouter, Mock), context (AGENTS.md), sandbox detection, ratatui TUI stub, and `nib doctor` to Rust.
- Removed Python core (`src/nib/`, `pyproject.toml`); `nib chat` / `nib run` run in-process.
- 15 Rust tests passing; `task check` green.
