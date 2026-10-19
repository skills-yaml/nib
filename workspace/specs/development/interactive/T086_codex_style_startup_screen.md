# T086: Codex-Style Startup Screen

**Status:** Development
State: development
Primary Feature: interactive

## Authority and Scope

On 2026-10-19 the user asked for the TUI startup screen to look like Codex's:
a rounded box announcing an available update (`✨ Update available! <current> ->
<latest>`, the update command and a release-notes link), followed by an indented
`>_ nib (v<version>)` header with the working directory under it. Asked about the
existing Session and Keys hints, the user chose to keep them below the header.

In scope: the empty-session welcome in the TUI and the structured update facts
it needs from the updater. Out of scope: the plain-mode and stderr update line
(`[nib] Channel update available: ...`, which the release qualification script
reads), update checking, channels, workspace consent and workload state.

## Affected Areas

`src/updater.rs` (structured startup update), `src/chat/split_00.rs` and
`src/tui/mod.rs` (plumbing), `src/tui/state.rs` (`StartupWelcome`),
`src/tui/chrome.rs` (`empty_state_lines`), TUI render and updater tests, and the
user guide. Version mirrors, release ledger, catalog and memory records reconcile
delivery. No governed instructions change.

## Acceptance Criteria

- [ ] AC-1: An empty session renders `>_ nib (v<version>)` indented two columns,
  with the working directory aligned under the name, and no `Nib <version>` /
  `Working directory` lines.
- [ ] AC-2: When a channel update is available, a rounded box precedes the header
  with `✨ Update available! <current> -> <latest>`, `Run nib update to update.`,
  and `See full release notes:` with the channel's GitHub release page. Equal
  versions on one channel show short commits so the two sides differ. Without an
  update there is no box.
- [ ] AC-3: The box borders fit the widest content line by display width
  (the sparkle is two cells), and monochrome terminals render it unstyled.
- [ ] AC-4: The Session and Keys hints remain below the header; workspace
  consent, the stderr update line and its qualification check are unchanged.
- [ ] AC-5: Focused render and updater tests, the user guide and required final
  gates pass.

## Implementation Plan

1. Reserve a fresh patch from published development 0.5.1 and apply it once.
2. Add `StartupUpdate { current, latest, release_notes }` derived from the
   existing availability classification; keep the stderr line as is.
3. Pass it to the TUI; render the box, header and indented hints.
4. Update tests and the user guide; review; freeze and run `task verify`.

## Validation Gates

`task test:interactive` and `task test:updater` while iterating; `task check`,
`task docs:check`, `task versions:check` and frozen `task verify` before handoff.

## Risks and Compatibility

Presentation only. Long lines in narrow terminals are clipped by the existing
paragraph, as before. Reverting restores the previous welcome with no persisted
state to migrate.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-startup-screen | Compatible display change after published development 0.5.1; fresh 0.5.2 applied once at development start. |

## Memory Impact

Status: updated
Rationale: The explicit display preference is recorded in
[workspace/agents/memory/preferences.md](../../../agents/memory/preferences.md)
and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md).
