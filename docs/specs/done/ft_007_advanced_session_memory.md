# FT-007: Advanced Session Memory & Summarization

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Implement token budgeting and dynamic session memory management so the agent can handle very long-running tasks without blowing out the LLM context window.

## Problem Statement
The current `.nib/sessions/` store simply appends every message and tool observation. Over long sessions, this naive accumulation will exceed the context window of the LLM or incur massive token costs.

## Goals
- Implement a background summarization daemon that periodically rolls up older tool observations into concise "facts" or "completed tasks".
- Preserve the raw audit trail in `.nib/sessions/`, but inject the summarized version into the LLM context.
- Keep system instructions, `AGENTS.md`, and immediate context in the "hot" memory window.

## Scope
- Create a `context::compression` module (`src/context/compression.rs`) containing logic to measure message history token size (or character count as a proxy).
- When the history exceeds a threshold, invoke a summarization prompt to the LLM to summarize older messages.
- Update `AgentLoop` to utilize this compression phase during `BuildContext`.
- Ensure raw session history is not deleted, only the runtime context block fed to the LLM is summarized.

## Acceptance Criteria
- Given a session with many messages exceeding the defined threshold, `maybe_compress_session` summarizes the old messages.
- The compressed summary is saved or managed without losing the full file-based audit trail.
- The LLM context receives the summarized history plus the most recent messages.
- `cargo fmt && task check` pass.
- All related unit tests pass.

## Affected Areas
- `src/context/compression.rs` (compression logic)
- `src/session/mod.rs` (adding summary fields if needed)
- `src/agent/loop.rs` (implementing the BuildContext memory optimization)

## Validation Gates
- `task check`
- `task test`
- Verification with a long session history to ensure the prompt size does not exceed limits.
