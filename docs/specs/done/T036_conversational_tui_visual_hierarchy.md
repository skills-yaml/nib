# T036: Conversational TUI Visual Hierarchy

**Status:** Done

**Related:**
[FT-019: Codex-Inspired Chat and TUI Interactions](../done/ft_019_codex_inspired_chat_and_tui_interactions.md),
[T018: ratatui Approval Flow](../done/T018_ratatui_tui_approval.md),
[T025: Interactive Chat and TUI Capability Parity](../done/T025_interactive_chat_tui_capability_parity.md),
[T028: Current-Session-First TUI and Slash-Command Completion](../done/T028_current_session_first_tui_and_slash_command_completion.md),
[T030: Unified Interactive CLI and Plain-Mode Fallback](../done/T030_unified_interactive_cli_and_plain_mode_fallback.md), and
[T031: FT-019 Interaction Model and Ledger TUI](../done/T031_ft019_interaction_model_and_ledger_tui.md)

## Summary

Refine nib's existing Ratatui interface into a quieter, conversation-first AI agent
experience. Preserve the authoritative session ledger, approvals, questions, queue,
and full `/status` diagnostics while making the normal TUI emphasize the user's
request, nib's response, and the next available action.

The visual direction is informed by an audit of nib's idle, command-discovery, and
completed-run states and by current Codex CLI and Claude Code CLI interaction
patterns: compact identity, generous transcript space, an unmistakable prompt, and
contextual controls instead of a permanently dense operator dashboard.

## Problem Statement

The TUI is functionally complete but its default hierarchy still reads like an
internal event log:

- the header exposes a full session UUID, full worktree path, and execution jargon
  that clips at ordinary terminal widths;
- routine run start, state-transition, terminal, and reconciliation records compete
  with the assistant answer even though the raw session audit already preserves them;
- an empty composer has no visible prompt or placeholder;
- the command picker is a large centered modal that obscures the transcript and
  repeats each command's insertion and usage text; and
- the footer shows the same dense key list regardless of whether nib is idle,
  running, awaiting input, or scrolled away from the tail.

This weakens scanability without adding authority: the detailed records remain
available in session persistence and explicit commands.

## Product Decisions

- The default TUI is conversation-first. It shows compact session and execution
  summaries; `/status`, `/permissions`, `/plan`, and the session audit remain the
  explicit detailed views.
- nib identifies its assistant turns as `nib`, not as a separate "coding" or
  "workload" persona.
- Routine run-start and state-transition events update live status but do not become
  transcript rows. Reconciliation remains visible as the terminal outcome; the raw
  lifecycle events remain persisted and auditable.
- Persisted plans use a one-line progress summary in the normal transcript. `/plan`
  retains the complete step list.
- The composer always has a `> ` prompt. When empty, it gives a short, muted cue that
  the user can ask nib to inspect, plan, or change the project.
- Command completion is a compact menu anchored above the composer. It shows one
  command signature and one description per row, with short selection hints.
- The footer is contextual: idle, active-run, and manual-scroll states advertise
  only relevant actions.
- One restrained 16-color palette is used when color is available. Role labels and
  textual state remain sufficient under `NO_COLOR`.

## Scope

- Add compact TUI-specific header and status formatting without changing the full
  `/status` output contract.
- Add a useful empty state, styled typed transcript rows, and a visible composer
  prompt/placeholder.
- Reduce routine lifecycle noise in the default persisted and live activity
  projection while preserving authoritative session data.
- Keep full plan detail on demand and show only plan progress in the normal ledger.
- Replace the covering command-completion modal with a compact bottom-anchored menu.
- Add contextual keyboard hints for idle, active-run, and scrolled transcript states.
- Add deterministic unit and Ratatui `TestBackend` coverage for the changed behavior.
- Update user documentation where the visible TUI contract changes.

## Non-Goals

- Cloning Codex or Claude visuals, branding, command syntax, or provider behavior.
- Changing the shared command registry, approval policy, tool authority, queue
  semantics, session persistence, or reconciliation rules.
- Removing lifecycle events or audit data from session JSON.
- Adding mouse interaction, syntax highlighting, Markdown rendering, a file tree,
  new overlays, or a new UI framework.
- Weakening redaction, bounded-output, Unicode-width, small-terminal, or `NO_COLOR`
  behavior.

## Acceptance Criteria

- [x] At ordinary terminal widths, the two fixed chrome rows present a compact nib,
      project, shortened session, lifecycle, provider/model, approval, sandbox, queue,
      plan, and context summary without exposing a full UUID or full worktree path.
- [x] `/status` continues to present the complete diagnostic chrome and effective
      execution posture.
- [x] An empty session shows a concise welcome cue and a visible `> ` composer prompt;
      entered text and the caret remain Unicode-width correct across two to six rows.
- [x] Assistant transcript rows use the `nib` label. Every activity remains
      distinguishable without color, and `NO_COLOR` retains the same information.
- [x] Routine run-start and state-transition events do not clutter the normal
      transcript, but persisted session events and live lifecycle state remain intact.
- [x] A completed run shows one reconciliation outcome rather than separate terminal
      and reconciliation success rows, and a persisted plan is summarized to one row;
      `/plan` retains full step detail. A terminal outcome without matching final
      reconciliation remains visible, including local failures after reopening.
- [x] Slash completion is anchored above the composer, does not cover most of the
      transcript, shows no duplicated signature column, and remains safe on small
      terminals.
- [x] Footer hints distinguish idle send, active-run queue/steer/cancel, and manual
      scroll-follow actions.
- [x] Existing approval and question docks, overlays, viewport behavior, queue
      semantics, redaction, and terminal restoration continue to pass their tests.
- [x] Focused interactive tests, `task docs:check`, `task check`, `task verify`, and
      `git diff --check` pass before completion.

## Affected Areas

- `src/interactive.rs` — display labels, compact TUI chrome, and quiet default
  activity projection while preserving explicit detailed formatters.
- `src/tui/mod.rs` — styled transcript, empty state, composer, contextual footer,
  compact completion menu, and cursor/layout calculations.
- `Cargo.toml` and `Cargo.lock` — directly use the Unicode segmentation dependency
  already used by Ratatui so wrapping and truncation preserve complete graphemes.
- `scripts/check-interactive-release.sh` — verify the revised completion and
  contextual scroll hints in the native terminal smoke.
- `docs/user/guide.md` — visible TUI layout and interaction cues.
- `docs/specs/README.md` — lifecycle inventory.
- `docs/specs/done/T036_conversational_tui_visual_hierarchy.md` — design and
  delivery contract.

## Implementation Plan

1. Separate compact TUI chrome from the existing detailed `/status` formatter and
   cover both contracts with deterministic tests.
2. Quiet the default activity projection, rename the assistant label, and keep full
   plan output behind `/plan`.
3. Introduce the restrained palette, styled transcript lines, welcome state, visible
   prompt, Unicode-correct caret offset, and contextual footer.
4. Replace the centered completion overlay with a bounded menu anchored immediately
   above the composer.
5. Update user documentation, capture the same three TUI states, compare them with the
   audit baseline, and correct visible hierarchy or clipping regressions.
6. Run spec-compliance review, code-quality review, focused tests, and all canonical
   completion gates.

## Validation Gates

1. Focused unit tests prove activity filtering, assistant naming, plan summary/full
   detail separation, compact chrome, and `NO_COLOR`-safe labels.
2. Ratatui `TestBackend` tests prove empty-state copy, composer prompt/caret placement,
   contextual footer copy, bottom-anchored completion, viewport preservation, docks,
   and small-terminal safety.
3. `task test:interactive` exercises the shared interactive and TUI regression suite.
4. `task docs:check` validates lifecycle placement and documentation links.
5. `task check` provides the required formatting and warning-denying static gate.
6. `task verify` runs the canonical complete local suite before completion.
7. `git diff --check` validates patch hygiene.

## Risks and Mitigations

- **Quiet projection hides required evidence:** Filter only routine presentation rows;
  keep raw session events untouched and keep failures, cancellation, approvals,
  questions, tools, compression, steering, and reconciliation visible.
- **Compact chrome obscures execution authority:** Retain approval and sandbox summary
  in the fixed status row and preserve full `/status` and `/permissions` output.
- **Prompt prefix breaks cursor placement:** Include the prefix width in wrap, height,
  and caret calculations and cover ASCII, Unicode, newline, and narrow-width cases.
  Wrap and truncate complete graphemes, including variation selectors and joined emoji.
- **Anchored completion clips on small terminals:** Derive its rectangle from the
  current frame with saturating bounds and preserve the existing small-terminal test.
- **Color becomes the only semantic channel:** Keep stable textual role/state labels,
  use only the terminal's 16-color palette, and test with `NO_COLOR` enabled.
- **Presentation filtering changes plain mode:** Keep compact chrome and styled
  rendering TUI-specific; validate existing plain-mode behavior unchanged.

## Review and Visual Evidence

Independent spec-compliance and code-quality reviews identified and corrected
grapheme splitting, missing persisted terminal failures, and suppression of a distinct
failure after apparent reconciliation. Terminal deduplication now requires matching
run/outcome evidence; an unmatched live failure also prevents queued work from advancing.

The 2026-09-12 manual Mock-only review captured idle, slash completion, and completed
session views at 100 columns, plus a resumed completed view at 80 columns. Compared
with the original audit, the prompt is visible, chrome retains the relevant execution
posture, completion sits above the composer, and the persisted conversation has one
final reconciliation and one plan-progress row. Local text captures are retained under
`target/tui-review-20260912-*.txt`. The live run also exposed adjacent plan-step
responses being concatenated; quiet lifecycle boundaries now finalize the preceding
assistant entry without adding presentation noise, with deterministic regression coverage.
The final live capture confirms separate assistant turns and a single final outcome.

## Completion Evidence (2026-09-12)

- `task verify` passed all 1,418 tests: 1,077 library, 86 CLI, and 255 integration
  tests, plus formatting and warning-denying Clippy. The paid live-provider entrypoint
  and separately invoked exact-release qualification entrypoint remained ignored.
- `task test:interactive`, `task docs:check`, and `git diff --check` passed.
- The locked optimized build passed. `task smoke:interactive:binary` passed the
  offline Linux PTY and redirected-mode cases, including exact terminal restoration.
- The smoke harness now treats CSI cursor boundaries as word separators for visible
  hints, scrolls after the intended resize, and pastes steering/queue drafts after a
  bounded wait for persisted planning. This preserves the plan-supersession and
  exact-cancellation assertions without changing production timing or authority.
  `task check` and `task test:interactive` passed again after those harness corrections.

No runtime persistence schema, tool authority, approval policy, or provider request
contract changed. T023 live qualification and FT-020 non-Linux production delegation
remain outside this completed presentation refinement.
