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
