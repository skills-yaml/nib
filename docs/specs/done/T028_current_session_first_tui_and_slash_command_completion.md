# T028: Current-Session-First TUI and Slash-Command Completion

**Status:** Done
**Related:** [T025](../done/T025_interactive_chat_tui_capability_parity.md), [T018](../done/T018_ratatui_tui_approval.md), [FT-011](../done/ft_011_llm_streaming_and_tui.md), [FT-013](../done/ft_013_advanced_session_memory.md)

## Summary

Make the TUI center the active session instead of permanently centering the session
browser. The main pane must show the active session's persisted conversation together
with new live events. Previous sessions remain available through an explicit,
preview-first session switcher, but browsing a previous session must never change the
active workload until the user confirms the switch. When the composer starts with `/`,
show bounded, keyboard-driven command completion sourced from the shared interactive
command vocabulary.

## Problem Statement

The current TUI gives most of the terminal to an always-visible list of session IDs and
limits the live stream to a fixed-height pane. This makes session administration more
prominent than the conversation the user is actively conducting. Resuming a selected
session changes the active session ID, but the main view is not rebuilt from that
session's persisted history, so the user does not regain the visual context of the
work they chose to resume.

The composer also accepts slash commands without discovery or completion. Users must
remember exact command names and fixed subcommands, while the parser, help text, and
TUI input behavior can drift because they are represented separately.

## Product Decisions

- The TUI is current-session-first. The active session timeline receives the primary
  screen area; historical session navigation is not permanently visible.
- `/session` is the explicit entry point to session history in the TUI. Opening,
  navigating, or previewing the switcher is read-only and does not change the active
  session.
- A previous session becomes active only after an explicit resume confirmation. The
  confirmed switch reloads a fresh, bounded projection from the authoritative
  `SessionStore`; it does not merge the previous pane's transient output into the
  resumed session.
- An active agent run cannot be moved to another session. The user must let it finish
  or cancel and reconcile it before confirming a switch.
- Typing `/` in the composer opens command completion. Completion is derived from one
  shared command registry used by parsing and help output, while the popup itself is a
  TUI presentation concern.
- `nib chat` and `nib tui` remain capability-equivalent. Line mode presents session
  selection, preview, confirmation, and command completion as bounded prompts instead
  of duplicating the TUI's overlays and key bindings.

## User Experience

### Default session view

- The header identifies the active session and whether its worker is idle, running,
  awaiting input, or reconciling.
- The main pane shows a bounded projection of persisted messages and relevant plan,
  tool, and lifecycle state for the active session, followed by live events from the
  current turn.
- On launch with `--session <id>`, the requested session is loaded into this view before
  the composer accepts a new turn. Launch without `--session` retains the existing new-
  session behavior.
- The composer remains the default focus. Removing the permanent session list must not
  regress approvals, questions, model selection, cancellation, or repeated turns.

### Explicit session switching

- `/session` opens a bounded session switcher overlay with the current session marked
  and previous sessions ordered deterministically, most recently updated first. An
  exact-ID field keeps sessions outside the bounded visible candidate window reachable.
- Moving through candidates may show a bounded preview containing enough context to
  distinguish sessions, including the session ID, last activity, latest user goal or
  message, plan/outcome summary, and a bounded transcript tail when available.
- Preview and cancellation leave the active session and current transcript unchanged.
  Submitting `/session` consumes that command exactly like every other submitted slash
  command; while its overlay is open, session navigation neither accepts nor mutates a
  separate composer draft.
- Activating a candidate requires a distinct confirmation that names both the current
  and target session.
- After confirmation, the TUI re-reads the target session, changes the active session
  exactly once, rebuilds the main pane from that record, leaves unrelated composer
  state alone, and records a visible local status line. Switching sessions does not
  create a message, lifecycle event, or tool audit entry by itself.
- Missing, deleted, replaced, or corrupt session state fails closed with an actionable
  error and leaves the original session active.
- `/clear` continues to create and activate a fresh session. It must use the same view-
  reload boundary so stale output from the former session is not shown as part of the
  new one.

### Slash-command completion

- A completion popup opens when the composer contains a slash-command prefix and no
  higher-priority modal is active.
- Suggestions filter case-insensitively by the command token. They show the command,
  concise usage, and summary without covering approval or question modals.
- Up/Down changes the highlighted suggestion, Tab inserts the highlighted completion,
  Esc closes completion without clearing the draft, and Enter keeps its existing
  submit behavior for a complete command.
- The first version completes command names and fixed subcommands. Free-form values
  such as paths, MCP process arguments, skill sources, and model IDs are not guessed.
  Session selection remains in the `/session` switcher rather than exposing raw IDs as
  a long inline completion list.
- Suggestions and input remain bounded for large terminals, small terminals, long
  command lists, Unicode input, and the existing composer byte limit.
- An empty `/`, an unknown command, or ambiguous/incomplete arguments never execute an
  agent goal. The composer preserves the input and presents a command-specific error or
  completion guidance.

### Chat parity

- `/session` in chat opens the same deterministic, bounded candidate set, accepts a
  displayed number or exact session ID, shows the selected preview, and requires an
  explicit confirmation before changing the active session.
- Cancelling a chat session prompt leaves the active session unchanged. Confirmation
  re-reads the authoritative target immediately before activation, just like the TUI.
- When chat receives an incomplete slash-command prefix, it presents bounded choices
  from the shared registry. Selecting a command or fixed subcommand completes it through
  line-mode prompts; required free-form arguments remain user supplied.
- Chat and TUI may use different controls and presentation, but neither interface may
  expose a session-management or slash-command capability that the other lacks.

## Scope

- Replace the always-visible session list with a current-session timeline as the TUI's
  primary pane.
- Hydrate the main pane from the active persisted session on launch, confirmed resume,
  and `/clear`, then append live events without misattributing them across sessions.
- Add an explicit, preview-first, confirmation-gated session switcher invoked by
  `/session`.
- Introduce shared interactive command metadata for parser/help/completion consistency.
- Add TUI slash-command completion for command names and fixed subcommands.
- Add line-mode session selection and slash-command completion prompts backed by the
  same bounded candidate and command registries as the TUI.
- Preserve chat/TUI command grammar parity and all existing approval, question, model,
  worker cancellation, terminal restoration, persistence, and reconciliation behavior.
- Update end-user documentation for the new layout, session workflow, completion keys,
  and removed always-visible browser controls.

## Non-Goals

- Changing the profile-scoped session persistence format or adding a global session
  database.
- Renaming, merging, deleting, pinning, tagging, or searching sessions.
- Running multiple sessions concurrently in one TUI process.
- Switching away from a running session or transferring an active worker between
  sessions.
- Shell-style completion for arbitrary paths, skill sources, MCP arguments, or model
  IDs.
- Pixel-identical session or completion presentation and identical key bindings between
  line mode and the TUI.
- Redesigning the agent loop, plan authority, worktree ownership, or context
  compression.

## Acceptance Criteria

- [x] The TUI opens with the active session timeline as its primary pane and without an
      always-visible historical session list.
- [x] Existing sessions are projected into the main pane on `--session`, confirmed
      switch, and return to a previously used session, within explicit row and byte
      bounds.
- [x] Persisted history and current live events are visibly ordered and cannot leak
      across active-session changes.
- [x] `/session` opens a deterministic, bounded session switcher with the active session
      marked and useful preview metadata.
- [x] Browsing, previewing, and cancelling the switcher do not change the active session
      or mutate persisted workload state.
- [x] Resuming requires explicit confirmation, re-reads authoritative state, and fails
      closed without changing sessions if the target can no longer be loaded safely.
- [x] A session switch is rejected while a worker is active; cancellation completes
      reconciliation before switching becomes possible.
- [x] Session browsing and confirmation never edit composer state; submitting
      `/session` consumes only that submitted slash command, and cancellation does not
      discard any unrelated draft.
- [x] `/clear` activates a fresh session and resets the visible session projection
      without showing stale output from the previous session.
- [x] Typing a slash-command prefix opens bounded, keyboard-navigable completion for all
      registered command names and fixed subcommands.
- [x] Chat exposes bounded line-mode completion for the same command names and fixed
      subcommands without allowing incomplete input to become an agent goal.
- [x] Chat `/session` uses the same ordered, bounded candidates and preview metadata,
      requires confirmation, revalidates the target, and directs later turns only to
      the confirmed session.
- [x] Parser, help, and completion entries are generated from or validated against one
      shared command registry.
- [x] Approval, question, model-selection, and session-switcher overlays take priority
      over completion, with no keystroke delivered to more than one interaction layer.
- [x] Unknown or incomplete slash commands cannot fall through as agent goals.
- [x] Repeated turns, model selection, approval, questions, cancellation, clean exit,
      and terminal restoration retain their T025/T018 behavior.
- [x] The user guide documents the current-session-first layout, explicit resume flow,
      and slash-completion controls.
- [x] `task docs:check`, `task check`, and an independent `task test` pass.

## Affected Areas

- `src/interactive.rs` — shared command metadata, parser/help consistency, and the
  presentation-neutral session-selection effect.
- `src/tui/mod.rs` — current-session projection, layout/focus state, session switcher,
  resume confirmation, completion popup, and modal precedence.
- `src/session/` — read-only bounded metadata/projection helpers only if existing APIs
  cannot safely provide them; no persistence schema change is expected.
- `src/chat.rs` — bounded line-mode completion, session selection/confirmation, and
  repeated-turn routing after a confirmed switch.
- `docs/user/guide.md` and `README.md` — user-facing TUI behavior where applicable.
- Interactive parser, Ratatui `TestBackend`, direct-key-dispatch, session persistence,
  and raw-terminal smoke tests.

## Implementation Plan

1. Replace parallel command-name/help constants with a typed, ordered command registry
   that supplies parser aliases, usage, summary, and fixed-subcommand metadata.
2. Define a bounded active-session projection that reads persisted transcript and
   workload summaries and accepts later live events without duplicating or crossing
   session identities.
3. Rework the TUI layout and focus model around the active-session projection and
   composer while preserving modal-first input handling.
4. Add shared bounded session-candidate projection plus the TUI `/session` switcher,
   preview, confirmation state, stale-target revalidation, composer isolation, and
   active-worker guard.
5. Add TUI slash completion state, filtering, rendering, key handling, and
   small-terminal behavior from the shared registry.
6. Add chat-native session selection/confirmation and slash-completion prompts using
   those same shared registries.
7. Add deterministic unit/rendering/state-transition tests, then update the user guide
   and execute the canonical validation gates.

## Validation Gates

- Shared-registry tests prove every parser command and alias has non-empty help and
  completion metadata, every displayed completion parses under its documented grammar,
  and duplicate aliases fail validation.
- Ratatui `TestBackend` snapshots or structural assertions cover the default
  current-session layout, hydrated history, live-event append, completion filtering,
  constrained terminal sizes, overlays, and switch confirmation.
- Direct key-dispatch tests cover completion navigation, modal precedence, preview
  cancellation, composer isolation, stale or corrupt target failure, `/clear`,
  worker-active rejection, and confirmed resume.
- Scripted chat tests cover completion selection/cancellation, fixed-subcommand and
  free-form argument handling, session preview/confirmation/cancellation, stale target
  rejection, and a later turn landing in the newly active session.
- Session tests prove switch and preview are read-only, target state is re-read before
  activation, and events from one session are never rendered or persisted as another.
- Linux raw-terminal smoke covers launch into an existing session, `/` completion,
  preview/cancel, confirmed resume, a repeated turn after resume, approval/question
  interaction, cancellation/reconciliation, and clean terminal restoration.
- `task docs:check`.
- `task check`.
- Independent `task test`.

## Implementation Reconciliation (2026-08-19)

- `src/interactive.rs` now owns one typed, ordered command registry for aliases,
  parsing, help, usage, summaries, and bounded command/fixed-subcommand completion.
- The TUI now renders a session-ID-bound active timeline with separate bounded
  persisted and live projections. `/clear` and confirmed resume replace that complete
  projection, while every worker stream event carries its launch session ID and
  mismatched late events are ignored.
- `/session` opens a bounded, most-recent-first switcher with active-session retention,
  exact-ID access beyond the visible window, preview/cancel isolation, explicit
  confirmation, active-worker rejection, and a fresh authoritative reload.
- Preview candidates carry a semantic snapshot fingerprint. Confirmation rejects
  deletion, corruption, intervening revision, and valid same-ID replacement before
  changing the active session.
- Chat uses the same registry and session candidate/validation helpers for bounded
  completion and preview-confirmed switching. Exact persisted IDs take precedence over
  numeric display indexes, and later turns use only the confirmed session.
- One interaction-layer reducer preserves modal precedence for approval, question,
  model selection, session confirmation, session browsing, completion, and composer
  input. Unknown, empty, or incomplete slash commands remain command errors and never
  become agent goals.

## Validation Evidence (2026-08-19)

- A spec-compliance review identified stale-target, event-isolation, draft-contract,
  rendering, and precedence gaps; all were corrected and covered by deterministic
  parser, state, chat, and Ratatui tests.
- A separate quality review identified bounded-window reachability, off-screen
  selection, and numeric-ID ambiguity; exact-ID lookup, selected-window rendering, and
  exact-ID-first chat resolution were added, then independently re-reviewed as green.
- `task check:all-targets`, independent `task test`, and canonical `task check` passed.
- `task smoke:interactive` passed on Linux with a release binary and real pseudo-
  terminals. Its T028 scenario verifies `/` completion, session preview/cancel,
  confirmed keyboard navigation, a subsequent turn persisted only to the resumed session,
  cancellation/reconciliation, alternate-screen exit, and terminal restoration.
- `task docs:check` passed before reconciliation and was rerun after the lifecycle move.

## Risks and Mitigations

- **Transcript ambiguity:** Combining persisted history and transient stream events can
  duplicate or misorder content. Keep the projection session-ID-bound and define one
  explicit hydration-to-live boundary.
- **Accidental workload switch:** A browsing gesture could redirect later execution.
  Keep preview state separate from active state and require confirmation plus a final
  authoritative reload.
- **Input-routing conflicts:** Completion keys can collide with model, approval,
  question, or session overlays. Preserve a single documented modal-precedence order
  and test that each key is consumed once.
- **Large session cost:** Rebuilding a long transcript can cause slow draws or excessive
  memory. Reuse bounded session-detail conventions and render only a capped projection
  with visible truncation markers.
- **Chat parity drift:** TUI-specific session presentation could fork the shared command
  grammar. Keep effects presentation-neutral and completion metadata in the shared
  registry; limit TUI-only behavior to rendering and interaction state.
- **Stale selection:** Session files can change between listing, preview, and resume.
  Treat list metadata as advisory and re-open through `SessionStore` immediately before
  activation, leaving the original session active on error.

## Rollout Notes

This change affects interaction behavior but not persisted data. No migration or
feature flag is required. It is complete after implementation reconciliation,
independent two-stage review, canonical tests, documentation validation, and the Linux
raw-terminal smoke gate.
