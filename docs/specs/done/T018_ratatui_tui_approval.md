# T018: ratatui TUI and Live Approval Flow

**Status:** Done
**Related:** [FT-005](ft_005_pure_rust_core_migration.md)

## Historical Gap Scope (2026-06)

The TUI (invoked via `nib tui`) currently acts as a minimal session browser. To meet the Phase 4 exit criteria, it must support an approval modal for destructive tool execution.

## Historical Problem Statement (2026-06)

When the agent executes a destructive tool (e.g., `run_terminal`), `ToolExecutor` pauses and requests human approval. Currently, this reads from `tokio::io::stdin()`. If running inside the TUI, this mechanism is incompatible with `crossterm`'s raw mode and screen rendering.

## Acceptance Criteria

- [x] Add an `ApprovalChannel` or callback mechanism to `ToolExecutor` so it doesn't hardcode `stdin`.
- [x] In `nib tui`, implement a background task that runs the agent loop and sends `ApprovalRequest` messages to the UI thread.
- [x] Display an approval modal in `ratatui` showing the tool name and arguments.
- [x] Accept `Y` or `N` keystrokes in the modal to send the decision back to the `ToolExecutor`.

## Affected Areas

- `src/tools/executor.rs`: Make approval IO pluggable.
- `src/tui/mod.rs`: Add channel communication and modal rendering.

## Validation Gates

- Pass `task check`.
- (Manual) Running `nib tui` with a destructive tool prompts the modal and resumes upon approval.

## Reopened Audit (2026-07-15)

Scope: prove the existing pluggable approval channel and modal, add question/tool
lifecycle rendering, and cover approval/denial/resume behavior deterministically.

Affected areas: `src/tui/mod.rs`, approval/event models, agent event emission, and TUI tests.

Validation gates: handler/modal state tests, manual raw-terminal smoke, `task check`,
and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Run the agent on an owned worker, route approval/question requests through channels,
render bounded live lifecycle/detail views, and cancel/join cleanly on exit.

### Acceptance Criteria

- [x] `ToolExecutor` accepts a pluggable `ApprovalHandler`.
- [x] TUI worker requests are delivered to approval and question modals.
- [x] Explicit Y/N and answer/cancel keys resolve the pending request.
- [x] TUI exit cancels, reconciles, drains terminal events, and joins the worker.
- [x] Session details stay inside a bounded scrollable overlay.
- [x] Manual raw-terminal approval, denial, question, detail, and cancellation smoke is recorded.

### Affected Areas

`src/tools/executor.rs`, `src/tui/mod.rs`, `src/agent/loop.rs`, stream event types,
and TUI tests.

### Implementation Evidence

- `src/tui/mod.rs` implements `TuiApprovalHandler`, `TuiQuestionHandler`, modal-first
  input dispatch, `TuiAgentWorker`, bounded live output, and detail overlay.

### Validation Evidence

- `src/tui/mod.rs`: approval grant/denial, question free-form/choice/cancel,
  lifecycle rendering, bounded overlay, and
  `tui_shutdown_cancels_and_joins_a_worker_blocked_on_approval` tests.
- Manual `/usr/bin/script` PTY runs on 2026-07-15 verified detail open/close, `q`
  cancellation with `cancelled_by_user` reconciliation, plan denial, plan approval,
  and selectable question response without terminal errors.

### Validation Gates

- [x] Deterministic handler, modal, rendering, and shutdown tests exist.
- [x] Manual raw-terminal smoke.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Automated tests use Ratatui's `TestBackend` and direct key dispatch; the recorded PTY
smoke covers Linux/crossterm behavior. Non-Unix terminals remain platform-specific.
