# T053: Visible Stop Reasons

**Status:** Done

State: done
Primary Feature: interactive

**Related:** [T050](T050_plan_free_answers_decision_prompts_and_run_outcomes.md),
[T026](../llm-providers/T026_actionable_redaction_safe_llm_failure_reporting.md),
[T047](T047_user_interaction_harmonization.md),
[T005](../agent-runtime/T005_full_runtime_state_machine_and_lifecycle.md),
[T025](T025_interactive_chat_tui_capability_parity.md)

## Summary

When a run stops, the user always sees a safe heading, a reason, and a next
action. Internal outcome tokens and discarded worker errors must not be the
only visible result.

## Problem

T050 maps structured `Reconciled` outcomes to a heading and next action. That
mapping is skipped on several stop paths, so the TUI and other surfaces look
like the agent died with no explanation.

Observed defects:

- The TUI worker maps any `run_agent_loop` `Err` to `StreamEvent::End("local_error")`
  and drops the error string (`src/tui/mod.rs` `safe_agent_error_stream_event`).
- `apply_stream_event` renders `End(reason)` as the raw token with an empty body.
  Tests assert the title `local_error`.
- `terminal_outcome_message` is used for `Reconciled` events, not for worker
  `End` events, join failures, or several one-shot/plain/gateway fallbacks.
- Plain and `nib run` can still print `Agent run failed: {outcome}` when
  `user_failure_report()` is absent.
- An unresponsive TUI worker shutdown can abort the whole TUI with an
  `io::Error` instead of a session-visible stop reason.
- Session reload can reconstruct the same unexplained token.

T026 still forbids showing provider prose, secrets, paths, or raw `Err` text.
The gap is presentation of *already-safe* stop classes, not weaker redaction.

Who is affected: anyone using TUI, plain chat, `nib run`, MCP/gateway, or
session reload after a non-reconciled stop.

## Scope

TUI, plain chat, `nib run`, gateway/MCP status, and session-reload presentation of
terminal stop reasons. Persistence tokens and T026 redaction stay.

## Goals

- Every stop the user can observe has a concise heading, a reason, remaining
  work when applicable, and a next action when one exists.
- TUI, plain chat, `nib run`, MCP/gateway status, and session reload show the
  same safe meaning for the same outcome token.
- Machine tokens and exit codes stay stable. Display text never becomes the
  authority for plan, approval, or verification state.
- Typed LLM failures keep T026's incident report. Do not replace it with a
  generic stop line.
- Raw error strings, provider payloads, and internal tokens such as
  `local_error` are never the only user-visible explanation.

## Non-Goals

- Inferring a cause from provider prose or reclassifying failure as assistant
  speech.
- New outcome authority, new exit-code table, or changing T047 cancel/quit keys.
- Fixing the Windows MCP stack overflow owned by T051.
- Splitting `src/tui/mod.rs` (T043) except as needed to call the shared mapper.
- Automatic retry or doctor runs.

## Implementation Plan

1. Extend `terminal_outcome_message` for `local_error` and unresponsive shutdown.
2. Render `StreamEvent::End` through that mapper; keep machine tokens on the wire.
3. Replace token-only `Agent run failed` fallbacks in chat, run, and gateway.
4. Surface unresponsive TUI shutdown as a mapped timeline event.
5. Update tests that asserted a bare `local_error` title.

## Design

### One presentation path

Extend the existing `terminal_outcome_message` mapper so every user-visible
stop goes through it (or T026's report for typed LLM failures). Cover at least:

- All T050 reconciled outcomes (unchanged copy unless a defect is proven).
- `local_error` and other previously unmapped tokens: heading "Run stopped",
  reason that the session was saved, next action inspect `/status` then retry
  or `nib doctor`.
- Unresponsive worker shutdown / join failure: heading that the run could not
  be stopped in time, next action inspect `/status` before sending more work.
- Duplicate `End` after a `Reconciled` event with the same token remains
  suppressed so the user sees one terminal result.

`StreamEvent::End(reason)` must render the mapped heading and detail, never
the bare token.

### Worker and loop errors

When the agent loop returns `Err` or the TUI/plain worker dies:

1. Prefer a typed `StreamEvent::Failure` when an `LlmError` is already known.
2. Otherwise emit a terminal event whose outcome token is a stable class
   (`local_error`, or a new named token for unresponsive shutdown if the
   current string error is the only signal). Persist that token on the session
   the same way other terminals are persisted.
3. Do not forward the raw `Err` string to the transcript, stdout, or gateway.
4. After the terminal event, the interactive session stays ready for the next
   user input unless the TUI itself cannot restore the terminal.

If the loop already reconciled with a structured outcome, that outcome is the
user-visible reason. A later persistence or lease error must not hide it
behind a bare `local_error`; show the known outcome and, if needed, one extra
safe line that the final save failed.

### Other surfaces

- Plain chat and `nib run` must not fall back to `Agent run failed: {token}`
  when the mapper or T026 report can speak.
- Gateway/MCP status uses the same heading and next action, still redacted.
- Reloaded timelines project the mapped message from persisted outcome tokens,
  not from discarded worker stderr.

### Documentation

User guide: a stopped run always states why and what to do next; `/status`
remains the durable inspection command. Append a dated T050 supersession note
that T050's mapping applies to worker `End` and `Err` paths, not only
`Reconciled` events. Do not rewrite T050 history.

## Alternatives

- Print the raw `Err` in the TUI. Rejected: violates T026.
- Map only `End("local_error")` in the TUI. Rejected: plain, `nib run`,
  gateway, and reload would still show tokens.
- Convert every loop `Err` into `Ok(summary)` in the agent loop. Useful later,
  but larger than the presentation hole; this spec requires the visible
  meaning first and allows the loop to start emitting structured summaries
  where it already has the class.

Chosen: shared mapper on every observer path, plus structured terminal events
for worker death, without weakening redaction.

## Affected Areas

- `src/tui/mod.rs`: worker error event, `End` rendering, shutdown timeout.
- `src/interactive/mod.rs`: `terminal_outcome_message`, `apply_stream_event`,
  plain status projection.
- `src/chat/mod.rs`, `src/run.rs`, `src/integrations/gateway.rs`: remove token-only
  `Agent run failed` fallbacks.
- `src/agent/loop/mod.rs`: emit a terminal event on `Err` when no summary exists;
  do not hide a prior reconciled outcome.
- Tests that currently assert title `local_error` or `[stream ended] local_error`.
- `workspace/docs/user/guide.md`, this spec, T050 supersession note, `workspace/specs/README.md`.

## Acceptance Criteria

- [x] A TUI worker `Err` shows the mapped stop heading and next action, not
      `local_error` as the only text.
- [x] `StreamEvent::End` for every mapped token shows heading plus detail; the
      bare token is not the activity title.
- [x] Duplicate `End` after `Reconciled` with the same token does not print a
      second final result.
- [x] Typed LLM failures still show T026's incident report, then at most one
      matching terminal line.
- [x] A prior reconciled outcome remains visible if a later save or lease error
      occurs; the extra failure is a safe extra line, not a replacement token.
- [x] Unresponsive worker shutdown leaves a session-visible mapped reason when
      the terminal can still be restored.
- [x] Plain chat, `nib run`, and gateway/MCP status use the same mapped meaning;
      no `Agent run failed: {token}` when a mapped message exists.
- [x] Session reload of the same outcome shows the same heading and next action.
- [x] Raw `Err` text, provider payloads, and secrets do not appear on any
      surface.
- [x] Focused TUI/interactive/runtime tests, `task check`, `task docs:check`,
      and `task verify` pass. Native Windows `task test` remains required
      before done.

## Validation Gates

- Unit tests for mapper coverage of `local_error`, unresponsive shutdown, and
  every T050 token.
- TUI test: worker `Err` with a sentinel payload must not appear; mapped copy
  must.
- Plain and `nib run` tests: no token-only `Agent run failed` fallback.
- Reload projection test for a persisted `local_error` (or successor token).
- `task check`, `task docs:check`, `task verify`.
- Native CI (Linux, macOS, Windows) before moving to done.

## Risks

- Showing a generic "Run stopped" for every unclassified `Err` is weaker than
  a typed class, but better than a silent death. Add named tokens only when
  the loop already knows the class.
- TUI shutdown timeout may still have to abandon an unresponsive thread. The
  outer restoration guard remains; this spec requires a visible reason, not
  a new way to kill Rust threads.
- Mapper copy changes can break string-matching tests. Update those tests to
  assert heading/detail, not tokens.

## Rollout

Land as one behavior-preserving presentation fix on `development` after
verification. No config flag. No session migration: old `local_error` tokens
gain mapped copy on display.

## Open Questions

None blocking. If implementation finds a stable unresponsive-shutdown token
cleaner than reusing `local_error`, add it to the mapper in this spec rather
than inventing a parallel message helper.

## Validation (2026-09-24)

- Unit tests: `stream_end_maps_local_error_instead_of_showing_the_token`,
  timeline End mapping, unresponsive-worker mapped heading.
- `task docs:check` passed.
- `task verify` passed on Linux after mapping copy assertions were updated.
- Native interactive release smoke synchronization now waits for mapped terminal
  headings, including `Run completed` and `Tool failed`, rather than internal
  outcome tokens.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | historical | Retrospective classification of the preserved pre-v7 outcome; no new bump or release identity is inferred. |

## Memory Impact

Status: none
Rationale: This historical outcome is preserved; migration adds no new durable decision for this spec. Existing dated memory evidence remains authoritative.
