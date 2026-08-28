# T032: FT-019 Explicit Compaction and Session Background Commands

**Status:** Done
**Parent:** [FT-019](../development/ft_019_codex_inspired_chat_and_tui_interactions.md)
**Related:** [T003](../development/T003_context_engine_with_dynamic_compression_and_session_management.md), [T012](T012_toolset_expansion.md), [FT-017](../development/ft_017_managed_process_supervisor.md), [T031](T031_ft019_interaction_model_and_ledger_tui.md)

## Summary

Replace FT-019's placeholder `/compact`, `/ps`, and `/stop` behavior with shared,
presentation-neutral operations over the existing context engine and durable task
store. Explicit compaction must preserve raw history. Background inspection and
cancellation must be restricted atomically to work owned by the active session.

## Scope

- Add an explicit compaction run mode that uses the configured production LLM adapter,
  acquires the normal exact-session run lease, records exact run start/terminal and
  compression evidence, and never creates a synthetic user or assistant message.
- Reuse T003's summary persistence and raw-history retention while allowing an explicit
  request to bypass only the automatic threshold. Compression-disabled configuration,
  empty history, provider failure, concurrent history mutation, and cancellation remain
  fail-closed and produce truthful outcomes.
- Add a bounded durable-task projection that exposes only task ID, kind, status, and
  timestamps for records whose persisted job owner is the active session.
- Add an atomic session-scoped cancellation API. An exact task ID belonging to another
  session must be indistinguishable from an unavailable task and must never be cancelled
  through the interactive command.
- Make `/ps` list only the active session's durable background work. Make
  `/stop [task-id]` require an exact task ID to mutate; without an ID it lists eligible
  work and explains the exact follow-up command.
- Route all three commands through the existing shared command registry and effect
  path so plain/chat and TUI have identical parsing, output, ownership, and persistence.
- Update FT-019 reconciliation and user/technical documentation after validation.

## Non-Goals

- Exact-run steering, queued-turn policy changes, or a new live agent-control channel.
- Cancelling foreground runs; existing Ctrl+C/TUI cancellation remains authoritative.
- Listing or stopping work owned by another session, profile, or project.
- Displaying durable command text, prompts, results, errors, worker PIDs, or raw task
  records in the interactive projection.
- Changing FT-017's platform containment guarantees or durable-worker lifecycle.

## Acceptance Criteria

- [x] `/compact` runs through the exact active session and reports either the bounded
      before/after budget or a truthful no-op reason without deleting raw messages.
- [x] Explicit compaction bypasses the automatic threshold only; disabled compression,
      missing history, provider failures, cancellation, and concurrent mutation remain
      bounded, redaction-safe, and terminally reconciled.
- [x] Explicit compaction persists one compression event when it changes the summary,
      creates no synthetic chat message, and records one matching run start/terminal.
- [x] `/ps` displays a bounded safe projection of only durable work owned by the active
      session in both renderers.
- [x] `/stop <task-id>` cancels only an active-session-owned durable task through the
      durable store's existing cancellation/reconciliation authority. Foreign, missing,
      malformed, and terminal task IDs fail closed without mutation.
- [x] `/stop` without an ID never performs a bulk cancellation and gives deterministic
      exact-ID guidance derived from the same session-owned projection as `/ps`.
- [x] Shared registry help, completion, parser, reducer, plain/chat, and TUI tests prove
      command parity and remove the obsolete capability-gate messages.
- [x] T003, FT-017, and FT-019 are reconciled with the delivered capability and no
      documentation claims exact-run steering is implemented by this task.
- [x] `task test:interactive`, focused context/durable tests, `task check`,
      `task check:all-targets`, `task coverage`, `task build`,
      `task smoke:interactive`, and `git diff --check` pass on the reconciled tree.
- [x] Independent spec-compliance review followed by code-quality/security review finds
      no unresolved blocking or high-severity issue.

## Affected Areas

- `src/context/compression.rs`
- `src/agent/loop.rs`
- `src/interactive.rs`
- `src/chat.rs`
- `src/tui/mod.rs`
- `src/daemons/workload.rs`
- Context, durable-task, interactive, and CLI integration tests
- `docs/user/guide.md`, `docs/tech/architecture.md`, and parent development specs

## Validation Gates

- Unit tests cover forced versus threshold-triggered compression, disabled/empty
  behavior, raw-history retention, and compare-and-swap rejection.
- Agent-loop tests cover exact run identity, no synthetic messages, emitted compression,
  cancellation, and safe provider failure for explicit compaction.
- Durable-store tests cover session filtering and atomic same-session versus foreign
  cancellation, including a task that changes terminal state during cancellation.
- Shared parser/effect tests cover `/compact`, `/ps`, `/stop`, and `/stop <task-id>`;
  redirected plain and Ratatui tests prove renderer parity and bounded output.
- Canonical Task gates listed in the acceptance criteria run after focused validation.

## Risks and Mitigations

- **Cross-session cancellation:** task ownership is checked while the durable record is
  locked in the same mutation that records cancellation; a preceding global lookup is
  never treated as authority.
- **Sensitive task projection:** the interactive view is a dedicated allowlisted type
  and cannot serialize command, prompt, result, error, worker, or lease fields.
- **Manual compression corrupts history:** explicit mode reuses T003's compare-and-swap
  summary publication and never removes or rewrites raw messages.
- **UI blocking during stop:** cancellation uses the existing bounded durable wait and
  reports the resulting authoritative status; a future asynchronous presentation may
  improve responsiveness without changing ownership semantics.
- **Feature-scope confusion:** exact-run steering remains a separate child task and is
  not implied by the compact/background command rollout.

## Implementation Plan

1. Extend compression with a force-threshold request while retaining every existing
   persistence and validation boundary.
2. Add an explicit compact agent-run mode and presentation-neutral command effect.
3. Add session-owned durable list/cancel APIs and bounded interactive formatting.
4. Reconcile tests, docs, parent specs, and run two-stage review plus canonical gates.

## Validation Evidence (2026-08-26)

- Exact-session explicit compaction, raw-history retention, compare-and-swap
  publication, session-owned task projection, and atomic cancellation regressions pass.
- Profile-drift, plain cancellation, compaction terminal-fence, undrained-channel, and
  missing/foreign/corrupt-record privacy regressions pass after review reconciliation.
- Independent spec-compliance and code-quality/security reviews report no unresolved
  blocking or high-severity finding.
- `task test:interactive` passed 135 focused tests; `task check`, `task test`,
  `task check:all-targets`, `task docs:check`, the locked optimized build,
  `task smoke:interactive`, and `git diff --check` pass on the reconciled tree.
- Runtime line coverage passes at 85.71% (82,695 / 96,482).
