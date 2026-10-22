# T086: Codex-Style Startup Screen

**Status:** Test
State: test
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
user guide, and `scripts/check-interactive-release.sh`, whose footer-word
normalization now matches bytes (`LC_ALL=C`): under a UTF-8 locale GNU sed left
escapes unstripped on lines carrying the case's invalid UTF-8 paste, failing
`Ctrl+End follow` locally on the base 0.5.1 binary as well. Version mirrors, release ledger, catalog and memory records reconcile
delivery. No governed instructions change.

## Acceptance Criteria

- [x] AC-1: An empty session renders `>_ nib (v<version>)` indented two columns,
  with the working directory aligned under the name, and no `Nib <version>` /
  `Working directory` lines.
- [x] AC-2: When a channel update is available, a rounded box precedes the header
  with `✨ Update available! <current> -> <latest>`, `Run nib update to update.`,
  and `See full release notes:` with the channel's GitHub release page. Equal
  versions on one channel show short commits so the two sides differ. Without an
  update there is no box.
- [x] AC-3: The box borders fit the widest content line by display width
  (the sparkle is two cells), and monochrome terminals render it unstyled.
- [x] AC-4: The Session and Keys hints remain below the header; workspace
  consent, the stderr update line and its qualification check are unchanged.
- [x] AC-5: Focused render and updater tests, the user guide and required final
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

## Validation Evidence

AC-1 to AC-3: `empty_state_boxes_the_update_above_the_codex_style_header` asserts
the exact box, header and directory rows and each box row's display width;
`startup_update_names_versions_and_the_channel_release_page` and the extended
manifest test cover version and commit labels and channel URLs; the session
render test asserts no box without an update. AC-4: the hints test, the unchanged
stderr notice test and the passing interactive release smoke. AC-5: `task check`
and full `task verify` (NIB_REQUIRE_BWRAP_TESTS=1) passed on 94e0c17; the later
ff70dee changes only the smoke script and this spec, and the full interactive
release smoke passed locally on a rebuilt ff70dee binary.

## Review

Independent exact-candidate review of 82aadb7..ff70dee: APPROVE, no blocking
findings. Two low findings are accepted follow-ups, agreed as non-blocking by the
reviewer: (1) below about 71 columns the update box is clipped on the right, as
the Risks section already records; a later change can drop the border when the
box does not fit; (2) no test asserts span colours for the box in colour and
monochrome modes. Two informational notes (a duplicate manifest fetch that
predates this change; no equal-version render test) need no change.

## Integration Evidence

Revision: fc10185118d1102fca7b9815d29652e03df6b742
Outcome: passed

The user said "merge" on 2026-10-22. PR #68 was merged into shared `development`
pinned to the verified head ff70dee as fc10185; the merged tree is identical to
ff70dee. PR CI on ff70dee passed Linux Validate (including the interactive
release smoke) and macOS Tests; Windows Tests were skipped by policy. This moves
T086 to test. Patch 0.5.2 was applied once; main delivery and publication remain
separate.

Memory impact of this integration record: none. The display preference is
unchanged; delivery evidence belongs in the spec.
