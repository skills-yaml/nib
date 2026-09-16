# T038: TUI Block Transcript and Key Contract

**Status:** Done

**Related:**
[T036: Conversational TUI Visual Hierarchy](../done/T036_conversational_tui_visual_hierarchy.md),
[T031: FT-019 Interaction Model and Ledger TUI](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T028: Current-Session-First TUI and Slash-Command Completion](../done/T028_current_session_first_tui_and_slash_command_completion.md),
[T037: TUI Cancellation Modal Cleanup](../done/T037_tui_cancellation_modal_cleanup.md),
[T039: Visible Tool Blocks and Explicit Approval Card](T039_visible_tool_blocks_and_explicit_approval.md),
[FT-019: Codex-Inspired Chat and TUI Interactions](../done/ft_019_codex_inspired_chat_and_tui_interactions.md)

## Summary

Raise the TUI from a labeled event log to a block transcript with Grok-quality
interaction basics: one mutating tool/assistant block, collapsed tool summaries,
a bordered composer, selectable foldable scrollback, and keys that never throw a
session away. Cache chrome so idle redraws do not reload configuration.

Grok is a behavioral reference for focus, blocks, and cancel/quit semantics. This
spec does not clone Grok's dashboard, themes, vim mode, command palette, markdown
rendering, or send-now-interject model.

## Problem Statement

T036 quieted chrome but the default view is still a string dump:

- tool lifecycle emits `requested`, `running`, and `ok` as three rows and dumps JSON;
- T031 required one mutating tool entry;
- slash completion treats Enter like Tab, against T028;
- idle `Ctrl+C` quits; `Esc` is a no-op;
- the transcript cannot be focused, selected, or folded;
- the composer has no widget chrome;
- every draw reloads config and recomputes execution posture.

## Product Decisions

- The transcript is a list of typed blocks. Tool lifecycle mutates one block.
- Collapsed tool blocks show a one-line summary. Raw JSON is expand-only.
- Tab moves focus between composer and transcript when completion is closed.
- Transcript focus: Up/Down select, Left/Right fold, Ctrl+Y copy the selected block.
- Printable characters while the transcript is focused return to the composer and insert.
- Tab inserts a slash completion; Enter submits a complete command (no trailing space).
- `Shift+Enter` and `Alt+Enter` insert a newline; `Ctrl+J` remains a newline.
- `Esc` never cancels a run. Double-Esc within 800ms clears a non-empty composer draft.
- `Ctrl+C` cancels an active run, or clears an idle non-empty draft. Idle empty `Ctrl+C` arms quit; a second `Ctrl+C` or `Ctrl+Q` within 1000ms quits.
- `Ctrl+Q` quits only after a second press within 1000ms. Idle empty `Ctrl+C` shares that confirm window.
- Composer is a bordered widget. Empty sessions show a startup welcome (version, working directory, optional update, session/worktree help, and keys) plus the prompt.
- Header/status are recomputed when session, lifecycle, queue, width, or a config-changing command changes — not on every idle frame.
- Session switcher rows show `display_name` when set, otherwise the abbreviated id.

## Scope

- Mutate one `ActivityEntry` through tool requested → running → terminal.
- Summarize common tool results (`list_directory`, `read_file`, `grep`, `run_terminal`) in the collapsed title; keep bounded detail for expand.
- Render activities as blocks with fold state and selection highlight.
- Add composer/transcript focus, fold, copy (OSC 52 plus a visible copied notice).
- Composer border, Shift/Alt+Enter newline, T028 Enter-on-slash, Esc/Ctrl+C/Ctrl+Q contract.
- Cache TUI chrome.
- Session switcher labels.
- Tests, user guide, and native smoke string updates.

## Non-Goals

- Markdown, syntax highlighting, mouse, themes, vim mode, command palette, dashboard.
- Changing queue vs steer semantics, approval policy, persistence, or reconciliation.
- Splitting `src/tui/mod.rs` (follow-up).
- Grok send-now / rewind / stash-restore (`Ctrl+S` remains steer).
- Plain-mode stream event formatting beyond the shared activity projection.

## Acceptance Criteria

- [x] Each live tool call is one activity: `requested` then `running` then terminal
      mutate the same entry, including concurrent or repeated calls with the same name.
- [x] Collapsed `list_directory` success shows an entry count, not a JSON object.
- [x] Expanding a tool block reveals bounded detail; collapsing hides it.
- [x] Tab with completion closed focuses the transcript; Tab from transcript returns to the composer.
- [x] Transcript Up/Down changes the selected block and keeps it in view; Left/Right toggles fold.
- [x] Ctrl+Y copies the selected block (OSC 52) and shows a copied notice.
- [x] Tab inserts slash completion; Enter on a complete insertion (no trailing space) submits it.
- [x] Shift+Enter and Alt+Enter insert a newline without submitting.
- [x] Esc never cancels a worker. Double-Esc within 800ms clears a non-empty idle draft.
- [x] Idle Ctrl+C clears a draft, or on an empty composer arms quit and a second press within 1000ms quits. Running Ctrl+C still cancels.
- [x] Ctrl+Q requires a second press within 1000ms to quit; idle empty Ctrl+C shares that confirm window.
- [x] Composer has a visible border; focused border is distinct from transcript-focused.
- [x] Empty session no longer shows the slogan block; the prompt and a startup welcome remain.
- [x] `format_tui_interaction_chrome` is not invoked on unchanged idle frames.
- [x] Session switcher list shows display names when present.
- [x] Existing docks, overlays, queue/steer, redaction, small-terminal, and `NO_COLOR` behavior remain.
- [x] Focused interactive tests, `task docs:check`, `task check`, `task test:interactive`, and `task verify` pass.

## Affected Areas

- `src/interactive.rs` — activity fold flag, mutating tool projection, tool summaries, session labels, optional chrome-key helper.
- `src/llm/types.rs`, `src/agent/loop.rs`, `src/tools/core.rs`, and
  `src/tools/executor.rs` — exact invocation identity on projected tool lifecycle
  and terminal-output events.
- `src/tui/mod.rs` — block render, focus, keys, composer chrome, chrome cache, completion Enter, copy.
- `docs/user/guide.md` — layout and key contract.
- `docs/specs/README.md` — lifecycle inventory.
- `scripts/check-interactive-release.sh` — visible hint strings.
- Unit and Ratatui `TestBackend` tests.

## Implementation Plan

1. Add `ActivityEntry.folded` and mutate tool lifecycle in `apply_stream_event`; summarize tool results.
2. Render activities as wrapped blocks with selection and fold; keep viewport row-based.
3. Add TUI focus, Tab, fold/copy keys, composer border, newline chords, Esc/Ctrl+C/Ctrl+Q.
4. Cache chrome on a state key; refresh after config-changing commands.
5. Update documentation, smoke strings, and tests.

## Validation Gates

- Unit tests for mutating tool projection and collapsed summaries.
- Ratatui tests for empty state, composer border, completion Enter vs Tab, footer/focus hints, fold, selection, small terminals.
- Key-dispatch tests for Esc, idle Ctrl+C, Ctrl+Q confirm, Shift+Enter.
- Chrome cache test: two identical idle frames do not reload config (or equivalent key-equality).
- `task test:interactive`, `task docs:check`, `task check`, `task verify`.

## Risks and Mitigations

- **Fold hides failures:** failed tools stay visible in the title (`failed`) even when folded; expand still shows the bounded error.
- **OSC 52 may not land:** always show the copied notice; copy is best-effort.
- **Double-press timing:** use the same 800ms/1000ms windows as Grok; first press is a status line, never a mutation except arming.
- **Smoke greps:** update `Commands` assertions if the completion title is removed.
- **Viewport row mapping:** map selected activity to rendered rows so selection cannot sit off-screen.

## Rollout Notes

Presentation and TUI key routing only. No persistence schema, tool authority, or
provider contract change. T023 and FT-020 remain out of scope.

## Final Reconciliation (2026-09-16)

T038 owns the block transcript, focus, folding, selection, copy, composer, and key
contracts. T039 owns the later under-composer lists, compact one-row chrome, channel
colors, waiting meter, workspace-consent prompt, and markdown speech presentation;
those decisions supersede this spec's earlier two-row chrome and markdown non-goal.

Live tool activities are correlated by nib's provider-neutral `ToolInvocationId`, not
by tool name. Requested, running, terminal-output, and completed events for repeated or
concurrent calls therefore update only their own block. The identifier remains internal
and is never rendered in the transcript. Focused tests exercise out-of-order completion
of two same-name calls, exact OSC 52 encoding, folded detail, row visibility, and the
focused composer border under `NO_COLOR`.

## Completion Evidence (2026-09-16)

`task check` passed formatting, installer syntax, and warning-denying Clippy. The
focused `task test:interactive` gate passed 16 steering, 57 shared-interaction, 95 TUI,
6 console, 26 plain-chat, 6 CLI, and one smoke-contract test. `task docs:check`, the
complete `task verify` gate, and `git diff --check` passed on the reconciled closure
branch before handoff.
