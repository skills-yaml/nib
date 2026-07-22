# FT-013: Advanced Session Memory & Summarization

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Implement token budgeting and dynamic session memory management so the agent can handle very long-running tasks without blowing out the LLM context window.

## Problem Statement
At the feature baseline, the session store appended every message and tool observation
without a bounded hot-context projection. Profile sessions retain that raw audit under
`.nib/profiles/<id>/sessions/`, while the reconciliation below records the shipped
compression behavior.

## Goals
- Implement threshold-driven summarization during context building so older tool
  observations become concise facts without blocking session persistence.
- Preserve the raw audit trail in the selected profile session, but inject the summarized version into the LLM context.
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
- `task check` passes.
- All related unit tests pass.

## Affected Areas
- `src/context/compression.rs` (compression logic)
- `src/session/mod.rs` (adding summary fields if needed)
- `src/agent/loop.rs` (implementing the BuildContext memory optimization)

## Validation Gates
- `task check`
- `task test`
- Verification with a long session history to ensure the prompt size does not exceed limits.

## Reopened Audit (2026-07-15)

Scope: honor the configured target ratio, trigger summarization without deleting
raw history, and prove the hot-memory prompt remains bounded.

Affected areas: `src/context/compression.rs`, `src/session/`, `src/daemons/`,
`src/agent/`, and long-session tests.

Validation gates: measured long-session tests, audit-retention assertions,
`task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Measure aggregate prompt size, summarize old session history at configured thresholds,
retain the raw audit, and project a bounded hot context into later turns.

### Acceptance Criteria

- [x] Compression threshold and target ratio are validated and applied.
- [x] Summaries persist with `summary_index`; raw messages remain intact.
- [x] Bounded context retains recent messages, AGENTS, skills, workload, memory, and critical edges.
- [x] Compression measurements/events are auditable.
- [x] Long-session behavior is deterministic under Mock LLM.
- [x] Final aggregate gates are green.

### Affected Areas

`src/context/compression.rs`, `src/context/budget.rs`, `src/session/`,
`src/agent/loop.rs`, and compression/session tests.

### Implementation Evidence

`maybe_compress_session` owns summary persistence; `build_bounded_runtime_input` owns
the non-mutating hot projection.

### Validation Evidence

`tests/compression_runtime.rs::compression_bounds_hot_context_and_retains_raw_audit_history`
and `tests/test_runtime_e2e.rs::compression_is_measured_audited_and_keeps_the_raw_transcript`.

### Validation Gates

- [x] Measured long-session and raw-audit retention tests exist.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Summary semantic quality is not benchmarked against live models; token-bound and audit
correctness are the shipped deterministic guarantees.
