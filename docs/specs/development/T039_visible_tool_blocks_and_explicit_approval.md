# T039: Visible Tool Blocks and Explicit Approval Card

**Status:** Development

**Related:**
[T038: TUI Block Transcript and Key Contract](T038_tui_block_transcript_and_key_contract.md),
[T031: FT-019 Interaction Model and Ledger TUI](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T018: ratatui Approval Flow](../done/T018_ratatui_tui_approval.md),
[FT-019: Codex-Inspired Chat and TUI Interactions](../done/ft_019_codex_inspired_chat_and_tui_interactions.md)

## Summary

Make live tool work and approval decisions unmistakable in the TUI. Tool blocks get a
Grok-style diamond marker, argument summary, and status coloring. The approval dock
becomes a bordered card that names the action, risk, and explicit Y/N choices without
covering the transcript or changing approval policy. Slash and path completion reserve
rows under the composer instead of overlaying the conversation. While a turn is running
or waiting, a one-row meter shows a spinner plus job, plan step, elapsed time, tokens,
and status.

## Problem Statement

T038 collapsed tools to one mutating line, but the default view still reads as a log
label (`tool  list_directory running`) without the path, command, or result shape the
user needs to scan. The approval dock is a yellow `approval  Action: …` dump with a
muted key hint; it does not look like a blocking decision, and the status row only
says `awaiting you`.

## Product Decisions

- Tool headers use a diamond marker plus the `tool` role label so they stay readable
  without color.
- Requested/running titles include a bounded argument hint (path, command, pattern).
- Completed titles keep that hint and a result summary (`N lines`, `N entries`,
  `exit N`). Failed tools stay red even when folded.
- Expanded tool bodies are indented with a left accent. Folded tools remain one line
  with `›` when detail exists.
- Approval is still a dock, not a covering modal. The dock is a bordered card titled
  `Approval required`. It states the intent in plain language (`Run this command`),
  shows the command or target on its own indented lines, and pins two labeled
  choices as the last rows: approve once, or deny. It does not dump `command=`
  metadata, network essays, or classifier reasons.
- `Y`, `Enter`, and `1` approve once. `N`, `Esc`, and `2` deny. Policy is unchanged:
  one-shot grant or deny; no always-allow in this slice.
- While an approval is pending, the TUI footer reads `WAITING APPROVAL` and
  shows the approval keys next to the approval mode.
- Questions use a bordered `Question` card. It states `nib is asking`, shows the
  question text, numbers the choices, and pins `Enter / 1-9` and `Esc skip`.
  Number keys submit the matching option. Free-form questions show a labeled
  answer field. The transcript stays visible. While a question is pending the
  footer reads `WAITING QUESTION`.
- `/` and `@` completion is a reserved band under the composer, not a `Clear` overlay
  over the transcript. The conversation stays visible and the composer stays above the
  option list. Slash option signatures start on the same column as the composer `/`
  and do not use a `>` caret; the selected row is emphasized in place. `/session`,
  `/model`, `/history`, and question option lists use that same under-composer
  band: no covering overlay, no caret, selected emphasis, optional
  `active`/`current` suffix.
- The first user goal assigns `display_name` when the session has no name.
  `/rename` remains authoritative and is not overwritten.
- A waiting meter appears between the transcript and the composer while a run is active
  or an approval/question is pending. It shows spinner, current job, plan step, elapsed
  time, a token estimate, and status.
- The conversation stays scrollable while an approval card is open. Wheel, PageUp,
  PageDown, and Shift/Ctrl+Up/Down move the transcript; Y/N still answer the card.
  Unmodified Up/Down in the composer remain draft history.
- The first chrome row shows the working directory and git branch on the left
  (branch colored when color is available) and the current model plus context
  usage on the right. The last row shows the command approval mode and the
  agent mode (`idle` / `execute` / `plan` / `compact`, or `WAITING APPROVAL` /
  `WAITING QUESTION` / `WAITING PERMISSION`). Session, worktree, sandbox, queue,
  and profile details stay in `/status`.
- User and nib speech blocks render markdown: headings, emphasis, lists, links,
  inline code, and fenced code with lightweight language coloring. Tool and
  thought blocks stay as structured transcript channels, not markdown.
- The transcript has three visually distinct channels: dim italic `thought` for
  internal planning, `◆ tool` work blocks, and a `nib` speech block for replies
  to the user. User turns stay `you`. Channels are separated by a blank row.
- An empty session starts with a left-aligned welcome: `Nib <version>`, the
  working directory, an update notice with `nib update` when a channel update is
  available, `/new` for a new session and worktree, `/session` to switch, and
  the most-used keys including double `Ctrl+C` / `Ctrl+Q` to quit.
- Idle empty `Ctrl+C` quits after a second press within 1000ms, matching
  `Ctrl+Q`. A running turn still cancels. A non-empty idle draft still clears.
- The first interactive start in a project asks permission to work in the
  working directory before any goal is accepted. The TUI shows a startup
  `Permission required` card with the directory and Y/N. A grant is persisted
  as `workspace.allowed` so later sessions skip the prompt. Decline quits.

## Scope

- Tool title composition with argument hints and diamond/accent rendering.
- Distinct thought / tool / speech transcript channels.
- Approval card layout, explicit choice rows, footer override, Enter/1/2 aliases.
- Question card layout, numbered choices, Enter/1-9/Esc, WAITING QUESTION footer.
- Compact header (folder/branch, model/context) and footer (approval mode, agent mode).
- Markdown rendering for user/nib speech, including fenced code.
- Layout reservation for completion under the composer and the waiting meter row.
- Tests and user-guide copy.
- Empty-session startup welcome and idle empty Ctrl+C quit confirm.
- Startup workspace permission card and persisted `workspace.allowed` grant.
- Compact header/footer chrome and markdown speech rendering.
- No change to `ApprovalDecision`, sandbox, or always-allow policy.

## Non-Goals

- Always-allow, scope widening, or YOLO mode UI.
- Full syntax-highlighter grammars, diffs-as-hunks, mouse, animation FPS, or
  command palette. Fenced code uses keyword/string/comment coloring only.
- Changing Y/N authority or making Esc park without answering (Grok parks; nib still
  denies on Esc, matching T018/T031).
- This slice supersedes T038's two-row chrome and the T038 markdown non-goal for
  user/nib speech.

## Acceptance Criteria

- [ ] Live tool headers render as `◆ tool  <name> <phase>` with an argument hint when
      the stream provided path/command/pattern.
- [ ] Collapsed completed `list_directory` still shows an entry count, not JSON.
- [ ] Expanded tool bodies are indented with a left accent and remain bounded.
- [ ] Failed tools are visually distinct without color (`failed` in the title) and red
      when color is available.
- [ ] The approval dock is a bordered card titled `Approval required` that states
      the intent (`Run this command` / `Read this file` / …), shows the command or
      path on its own lines, and pins `Y Approve once` and `N Deny` as the last rows
      even on a 40-column terminal. It does not render `command=` dumps.
- [ ] Transcript text above the dock remains visible at ordinary terminal sizes.
- [ ] `Y`, `Enter`, and `1` grant once; `N`, `Esc`, and `2` deny.
- [ ] The footer shows `WAITING APPROVAL` and the approval keys while the card is
      open; `NO_COLOR` still has the same words.
- [ ] Focused interactive tests, `task docs:check`, `task check`, and
      `task test:interactive` pass.
- [ ] Slash completion options render under the composer; conversation text above the
      input remains visible and is not cleared. Option signatures start on the
      same column as the composer `/` and have no `>` caret. Session, model,
      history, and question lists use that same selected-row style.
- [ ] The first user goal assigns `display_name` when unset; `/rename` is kept.
- [ ] The waiting meter shows spinner, job, step, elapsed time, tokens, and status
      while a run is active or the TUI is waiting.
- [ ] Wheel, PageUp/PageDown, and Shift/Ctrl+Up/Down scroll the transcript even while
      an approval card is open; unmodified composer Up/Down still recall draft history.
- [ ] The first row shows folder and branch on the left and model plus context
      usage on the right. The last row shows approval mode and agent mode.
      `WAITING APPROVAL` replaces the agent-mode token, not the folder/model fields.
- [ ] User and nib speech render markdown headings, lists, emphasis, inline code,
      and fenced code; tool/thought channels are unchanged.
- [ ] Internal thinking renders as `thought` (dim/italic), tool calls as `◆ tool`,
      and user-facing replies as a `nib` speech block with indented body text.
- [ ] A question dock is a bordered `Question` card that states `nib is asking`,
      shows the question, numbers options, and pins Enter/1-9 and Esc. The footer
      reads `WAITING QUESTION`.
- [ ] An empty session welcome shows `Nib <version>`, the working directory,
      `/new` and `/session` help, and the most-used keys. When an update is
      available it tells the user to run `nib update`.
- [ ] Idle empty `Ctrl+C` twice within 1000ms quits; a running `Ctrl+C` still
      cancels and a non-empty idle draft still clears. `/q` and `Ctrl+Q` still
      quit.
- [ ] First TUI start without `workspace.allowed` shows a `Permission required`
      card asking to work in the working directory. `Y`/`Enter` persist the
      grant; `N`/`Esc` quit. `--run` waits until the grant. Later starts skip
      the card.

## Affected Areas

- `src/interactive.rs` — tool argument hints, title composition, display_text,
  compact `TuiChrome`.
- `src/tui/mod.rs` — tool row styling, approval card, header/footer chrome, keys,
  below-composer completion layout, waiting meter, transcript wheel/key scroll,
  startup welcome, idle Ctrl+C quit, workspace permission card.
- `src/tui/markdown.rs` — speech markdown and fenced-code rendering.
- `src/config/mod.rs` — `workspace.allowed` grant.
- `src/chat.rs` / `src/updater.rs` — pass the startup update notice into the TUI;
  plain-mode TTY workspace consent.
- `docs/user/guide.md` — approval, tool-block, completion placement, waiting-meter,
  startup welcome, and Ctrl+C quit copy.
- `docs/specs/README.md` — lifecycle inventory.

## Implementation Plan

1. Compose tool titles with bounded argument hints across requested/running/terminal.
2. Render diamond + accent tool blocks with status coloring.
3. Replace the approval dock dump with a bordered explicit-choice card and status/footer.
4. Add Enter/1/2 aliases; cover with TestBackend and key-dispatch tests.
5. Reserve completion rows under the composer and stop overlaying the transcript.
6. Add the waiting meter row for job, step, time, tokens, and status.
7. Add the empty-session startup welcome (version, cwd, update, session/worktree, keys).
8. Let idle empty Ctrl+C share the Ctrl+Q 1000ms quit confirm.
9. Ask workspace permission on first interactive start and persist `workspace.allowed`.
10. Collapse chrome to one header row (folder + branch left, model + context right)
    and a footer of approval mode plus agent mode.
11. Render user/nib speech as markdown with fenced-code coloring.

## Validation Gates

- Unit tests for argument hints and diamond display text.
- Ratatui tests for the approval card title, choice rows, transcript visibility, and
  WAITING APPROVAL footer.
- Tests for compact header/footer chrome and markdown speech (headings, lists, code).
- Key tests for Enter/1 grant and 2 deny.
- `task docs:check`, `task check`, `task test:interactive`.

## Risks and Mitigations

- **Small terminals:** clamp the card so the transcript keeps at least three rows.
- **Secret leakage in argument hints:** reuse existing redaction/bounding helpers.
- **Enter collision:** approval owns input while the card is open, so Enter cannot
  submit the composer.

## Rollout Notes

Presentation and TUI key aliases only. T038 remains the interaction-contract owner.
T023 and FT-020 stay out of scope.
