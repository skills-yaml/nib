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

- [x] AC-1: Codex and legacy presets resolve actions by context; malformed and
  colliding overrides fail validation, with persistent set/unbind/reset and effective help.
- [x] AC-2: Running Enter steers, running Tab queues, completion takes precedence;
  modal answers and durable workload state stay authoritative.
- [x] AC-3: Copy, clear-view and composer editing operate on their intended targets.
- [x] AC-4: External editor success/failure restores the terminal and preserves the draft.
- [x] AC-5: Empty double Escape restores the latest user message in a new fork;
  original history remains intact, and post-branch execution state is not inherited.
- [x] AC-6: Legacy preserves prior defaults; tests, docs and independent review pass.

## Implementation Plan

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
records cannot credit new work. Published 0.4.0 is preserved; the independently reviewed atomic reservation returned fresh minor 0.5.0. The version owner applies it once before final integration artifacts.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-keymap-alignment | Additive keymap and changed defaults use the fresh 0.5.0 reservation from published 0.4.0, applied once by t084-keymap-version before integration artifacts. |

## Memory Impact

Status: updated
Rationale: Record the approved keymap and migration contract in
[workspace/agents/memory/decisions.md](../../../agents/memory/decisions.md) and
[workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md).

## Version Reconciliation

The public development manifest published 0.4.0 at
8c0eabb4d1ea768dee9b5d20872aa49a8181bc44 while this feature was being implemented.
T084 was withdrawn from that released membership. Independent prospective review
approved the bounded reservation read-basis adaptation at d978445; the same atomic
runtime transaction returned nib-keymap-alignment minor 0.5.0 from 0.4.0. Only that
reservation call used the committed publication-ledger read basis; no historical
integration ref, task ownership or worktree was changed. Version mirrors were
updated once before final integration artifacts. Source work also incorporates
remote development permission-rule revision 8c0eabb; combined review/gates renew.

## Implementation and Review Evidence

The initial source implementation passed task test:interactive. Review findings
about editor directory permissions, completion ownership, remapped action/context
availability and running file-completion steering were fixed with regressions.
The combined source includes development 8c0eabb and the reserved 0.5.0 version.
Independent review approved 9f1b9da; its complete task verify passed, including
1,452 library tests, CLI and integration suites, and documentation integrity.
Independent review also approved the fail-closed smoke follow-up at 5998fdc.
The reconciled final candidate requires renewed exact review and
final task verify, task build and task smoke:interactive:binary before handoff.
Local implementation does not establish shared integration, main delivery or
publication. Native Windows/macOS editor qualification remains separate.

Footer review found unconditional running hints in idle and modal contexts. Hints
now follow modal ownership, transcript focus and the effective composer/running
bindings, including overridden and unbound actions. Regression coverage exercises
each context. The docs-integrity gate also required the explicit Implementation
Plan heading; the existing ordered plan now uses that heading.

## Acceptance Evidence and Final Qualification

- AC-1: Configuration/keymap tests cover context availability, effective presets,
  override collisions, invalid writes, unbinding and reset. Footer regressions
  cover idle, running, transcript, completion/modal and customized hints.
- AC-2: Steering and shared-reducer tests cover exact-run fencing, Enter steering,
  Tab queue/completion precedence and modifier-safe approval ownership.
- AC-3: TUI tests cover composer actions, output/selection copy and view-only
  clearing without changing persisted session history.
- AC-4: Editor tests cover successful edits, failure and invalid-output recovery,
  private backups, symlink rejection and terminal resume on every error path.
  The native Unix release gate additionally exercises editor success/failure.
- AC-5: Fork tests preserve the original, restore the last user message, exclude
  prior execution authority and reject a concurrently running original.
- AC-6: Preset and command tests cover legacy migration/reset; user documentation
  describes effective bindings and rollback. Independent exact-candidate review
  and the full Task gate passed 9f1b9da. Final qualification renews after the
  bounded smoke-script follow-up and these evidence records.

The first native run established draft recovery and persisted preset changes,
but redraw-dependent waits did not fail their producer. Its overall success is
not accepted as qualification. Revised cases use bounded editor invocation and
persisted configuration synchronization, propagate all new wait failures, and
must pass against the clean, exact-revision release binary before delivery.
Local qualification is Linux; Windows/macOS native editor execution is not
claimed. No shared integration, main merge or publication is recorded here.
