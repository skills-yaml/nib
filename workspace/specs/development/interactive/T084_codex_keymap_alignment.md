# T084: Codex Keymap Alignment

**Status:** Development
State: development
Primary Feature: interactive

## Authority and Scope

User approved implementation of the proposed alignment on 2026-10-12.
Follow-up to [FT-019](../../done/interactive/ft_019_codex_inspired_chat_and_tui_interactions.md).
Codex is the default preset; legacy retains existing nib shortcuts. Context-aware
named actions route into existing audited queue, steering, cancellation and session
operations. No approval or question answer may be synthesized by a composer binding.

## Proposed Behavior

Enter sends when idle and steers while running; Tab completes an open suggestion
before queueing a running draft. Ctrl+O copies completed output; Ctrl+G uses VISUAL
then EDITOR with terminal restoration and draft recovery. Ctrl+L clears only the
visible transcript when idle. Empty-composer double Escape forks before the latest
user message and restores it for editing. Configurable bindings and /keymap show,
set, unbind and reset are persisted with validation. Legacy is available explicitly.
Ctrl+C cancels a running task, clears a nonempty idle draft, and exits when idle
and empty; shutdown always reconciles workers. Modal controls keep their ownership.
Standard composer editing replaces global Ctrl+A chat selection in the Codex preset.
Vim mode and local shell shorthand are separate features and excluded.

## Affected Areas

src/config, src/tui, src/interactive, session fork behavior, deterministic tests,
workspace/docs/user/guide.md. No governed instructions change.

## Acceptance Criteria

- [ ] AC-1: Codex and legacy presets resolve actions by context; malformed and
  colliding overrides fail validation, with persistent set/unbind/reset and effective help.
- [ ] AC-2: Running Enter steers, running Tab queues, completion takes precedence;
  modal answers and durable workload state stay authoritative.
- [ ] AC-3: Copy, clear-view and composer editing operate on their intended targets.
- [ ] AC-4: External editor success/failure restores the terminal and preserves the draft.
- [ ] AC-5: Empty double Escape restores the latest user message in a new fork;
  original history remains intact, and post-branch execution state is not inherited.
- [ ] AC-6: Legacy preserves prior defaults; tests, docs and independent review pass.

## Ordered Plan

1. Add validated keymap configuration and pure context resolver, then persistence commands.
2. Route TUI semantic controls through it; preserve modal precedence and queue/steer audit.
3. Add copy, view-only clearing, editing, editor recovery and fork-before-message support.
4. Cover observable success/error/context/migration paths and update help/user docs.
5. Resolve independent exact-candidate review; freeze and run native Task gates.
6. Local work remains development; confirmed shared development integration moves test;
   verified main merge and reconciled records move done. Publication is separate.

## Validation Gates

Use task test:interactive, task test:tui-shutdown and task test:runtime-e2e for
changed semantics; task docs:check and task versions:check for records. Finish
with task verify after independent interface/security review. Native terminal
smokes exercise editor restoration; unsupported host qualification is reported.

## Compatibility, Risks and Rollback

Default changes are visible in effective shortcut help; set tui.preset = "legacy"
to restore legacy behavior. Configuration changes are atomic. Context precedence
prevents queued prompts from deciding approvals. Editor commands are explicitly
user-configured local processes; drafts use private temporary files. Forked execution
records cannot credit new work. Shared 0.4.0 remains already applied with no second bump.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-catalog-refresh | Additive keymap and changed default share the atomically rechecked applied 0.4.0 minor reservation. |

## Memory Impact

Status: pending
Rationale: Resolve the stable keymap and migration contract after verification.
