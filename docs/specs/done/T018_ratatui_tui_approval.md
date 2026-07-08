# T018: ratatui TUI and Live Approval Flow

**Status:** Development
**Related:** [FT-005](../development/ft_005_pure_rust_core_migration.md)

## Scope

The TUI (invoked via `nib tui`) currently acts as a minimal session browser. To meet the Phase 4 exit criteria, it must support an approval modal for destructive tool execution.

## Problem Statement

When the agent executes a destructive tool (e.g., `run_terminal`), `ToolExecutor` pauses and requests human approval. Currently, this reads from `tokio::io::stdin()`. If running inside the TUI, this mechanism is incompatible with `crossterm`'s raw mode and screen rendering.

## Acceptance Criteria

- [ ] Add an `ApprovalChannel` or callback mechanism to `ToolExecutor` so it doesn't hardcode `stdin`.
- [ ] In `nib tui`, implement a background task that runs the agent loop and sends `ApprovalRequest` messages to the UI thread.
- [ ] Display an approval modal in `ratatui` showing the tool name and arguments.
- [ ] Accept `Y` or `N` keystrokes in the modal to send the decision back to the `ToolExecutor`.

## Affected Areas

- `src/tools/executor.rs`: Make approval IO pluggable.
- `src/tui/mod.rs`: Add channel communication and modal rendering.

## Validation Gates

- Pass `task check`.
- (Manual) Running `nib tui` with a destructive tool prompts the modal and resumes upon approval.
