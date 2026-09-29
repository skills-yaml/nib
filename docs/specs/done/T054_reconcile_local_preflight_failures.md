# T054: Reconcile Local Preflight Failures

**Status:** Done

**Related:** [T053](../done/T053_visible_stop_reasons.md), [T026](../done/T026_actionable_redaction_safe_llm_failure_reporting.md)

## Summary

Give a failed tool preflight a specific, durable stop reason and close proposed tool rows when a run ends. Deliver final stream events reliably.

## Problem and Evidence

The 2026-09-26 `analyze the repo` session (`d381db5b-8bf6-4512-b533-9097334d5328`) persisted four proposed tools and a transition to `user_approval`, then `local_error`. It recorded zero tool attempts. The `run_terminal` proposal requires `prepare_session_worktree` before tool execution. The absence of a managed worktree receipt makes that preflight the most likely failing operation, but the exact failing call cannot be proven from the saved record. `safe_agent_error_stream_event` discarded the error, so the historical filesystem/Git cause cannot be recovered. TUI proposals display running spinners, and session reload renders `run terminal: local_error` without guidance.

Other stop paths can lose a useful result: final `Failure`/`End` sends use `try_send` and ignore a full channel; a TUI worker panic exits through `join()?`; pre-run and persistence errors return raw `Err` to the worker. T053 gives generic mapped copy for `local_error`, but these paths can still show no specific cause or leave pending rows.

## Scope and Goals

- Reconcile managed worktree preflight failure with a stable `worktree_preparation_failed` outcome before any tool attempt, with no raw error in user content.
- Close every proposed/running tool row when a terminal event arrives without a matching tool completion.
- Map persisted terminal outcomes through the shared user-facing stop message.
- Give final loop `Failure` and `End` events a bounded delivery window and a queued fallback while the runtime remains active.
- Keep plan state, run terminal audit, and next-run recovery consistent.

## Non-Goals

- Reconstructing the discarded historical error or printing raw Git, filesystem, or provider errors.
- Retrying worktree creation or changing worktree ownership rules.
- Replacing all string-based agent loop errors with a new error type.

## Design

In `UserApproval`, catch `prepare_session_worktree` failure, record a bounded local preflight event containing only the stable stage, abandon the proposed batch, and transition to normal reconciliation with `worktree_preparation_failed`. The shared outcome mapper gives the user a concrete reason and directs them to `nib doctor` and Git/worktree inspection. Mark the outcome as a failure so the bound plan remains blocked rather than completed.

On terminal stream projection, change unmatched `requested` and `running` tool activities to `not run` or `stopped` before adding the terminal reason. Keep tool calls that already completed unchanged. Apply the same rule in plain and TUI presentation. Session reload projects `run_terminal` through the shared outcome mapper.

Give final loop events a bounded chance to enter the stream; if backpressure persists, queue their delivery for the running runtime. The TUI worker sends a final `End` after a successful loop return, and its timeline deduplicates matching terminal events. When a receiver is closed, persisted run terminal state remains the authority. Keep raw `Err` out of public stream events.

## Implementation Plan

1. Give managed worktree preflight a reconciled failure outcome and safe audit stage.
2. Map that outcome across live output, reload, and one-shot reporting.
3. Close unfinished tool rows on terminal events and suppress duplicate TUI `End` events.
4. Add deterministic regression tests and run the canonical quality gates.

## Affected Areas

- Agent loop: preflight transition, failure classification, final stream delivery.
- Interactive/TUI: pending tool rows and persisted terminal projection.
- Tests: preflight failure, terminal row closure, reload, and full-channel delivery.
- User guide: specific worktree stop guidance.

## Acceptance Criteria and Validation Gates

- [x] A failing managed worktree preflight makes zero tool attempts, records reconciliation and run terminal with `worktree_preparation_failed`, and blocks the active plan.
- [x] TUI and plain output show the safe worktree reason and next action; raw internal errors remain absent.
- [x] Pending tool proposals stop spinning after terminal failure; completed tools keep their results.
- [x] Reloaded `local_error` and worktree failure display mapped copy, not raw tokens.
- [x] A temporarily full but open stream receives the final `End` once drained.
- [x] Focused tests, `task check`, `task docs:check`, and `task verify` pass.

## Alternatives and Risks

Printing raw preflight errors would expose paths and environment details. A typed error hierarchy across every loop operation would be larger than this repair. The selected stage-specific outcome is safe and auditable, but does not identify the exact Git or filesystem failure; operators may still need local diagnostics.

## Rollout

No session migration. Existing `local_error` records gain mapped reload presentation. Machine outcome `worktree_preparation_failed` is additive.

## Open Questions

None blocking.

## Stop-Path Audit (2026-09-26)

- **Observed pre-execution stop:** tool proposals preceded every tool attempt. Managed worktree preparation is the most likely failing call. The raw cause is absent from the historical session.
- **Other admitted loop errors:** `?` propagation still reaches the worker as an unclassified string. The TUI maps these to a safe generic `Run stopped` result. A broader typed local-error taxonomy remains outside this task.
- **Before-run and final-save failures:** configuration, lease, or persistence errors may prevent a durable terminal record. The live TUI fallback remains visible; a failed store cannot promise a saved reason.
- **Worker panic or shutdown failure:** `join()` returns an outer TUI error. Unresponsive shutdown already has a mapped stop class under T053; a process-level panic may prevent a durable session update.
- **Backpressure and reload:** final stream delivery is now bounded with an active-runtime fallback, and the TUI worker adds a deduplicated final event. Reloaded `run_terminal` records use the same stop mapper as live output.

## Validation (2026-09-26)

The non-Git mock project regression produced `worktree_preparation_failed` with no tool-start or attempt events, a blocked plan, reconciliation, and one run terminal. Projection tests covered unfinished proposals, completed tools, historical `local_error`, and the new outcome. A full but draining stream received its final event. `task test:agent-context`, `task test:interactive`, `task docs:check`, and the exact-source `task verify` passed on Linux.
