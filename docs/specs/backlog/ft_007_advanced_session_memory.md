# FT-007: Advanced Session Memory & Summarization

**Status:** Backlog
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Implement token budgeting and dynamic session memory management so the agent can handle very long-running tasks without blowing out the LLM context window.

## Problem Statement
The current `.nib/sessions/` store simply appends every message and tool observation. Over long sessions, this naive accumulation will exceed the context window of the LLM or incur massive token costs.

## Goals
- Implement a background summarization daemon that periodically rolls up older tool observations into concise "facts" or "completed tasks".
- Preserve the raw audit trail in `.nib/sessions/`, but inject the summarized version into the LLM context.
- Keep system instructions, `AGENTS.md`, and immediate context in the "hot" memory window.
