# T092: Default Keymap Label and Workspace Skill Pins

State: test
Primary Feature: interactive

## Scope

Deliver the user-approved existing unstaged edits: display the Codex keymap
preset as Default in startup and contextual footer hints, and update the
workspace skill manifest and lockfile together. Follow up
[T084](../../test/interactive/T084_codex_keymap_alignment.md) and
[T086](../../test/interactive/T086_codex_style_startup_screen.md).
Preserve serialized preset names, key bindings, Legacy display, and workload state.
No governed instruction edits.

## Acceptance Criteria

- [x] AC-1: Startup and contextual hints display Default for the Codex preset.
- [x] AC-2: Legacy hints and configured bindings retain their behavior.
- [x] AC-3: All 20 direct skill pins agree with the lockfile; its additional
  design-domain-model entry remains a transitive dependency.
- [x] AC-4: Native mirrors agree on the single reserved patch and required gates pass.

## Affected Areas

skills.yaml, skills.lock.yaml, src/tui/chrome.rs, src/tui/tests/keymap.rs,
src/tui/tests/render.rs, Cargo.toml, Cargo.lock, the user guide, spec catalog and release ledger.
Consumers are startup rendering, footer hints and workspace skill installation.

## Implementation Plan

1. Check pins and published baseline; reserve a fresh patch with independent review.
2. Commit skill updates separately, then label changes, regression coverage and
   version/spec records. Preserve the original edits.
3. Review the exact candidate, run focused interactive checks then task verify,
   and merge into shared development through normal delivery protections.
4. Reconcile lifecycle after confirmed integration; main delivery is separate.

## Validation Gates

Check manifest/lock agreement, task test:interactive, task docs:check,
task versions:check, independent exact-candidate review, and frozen task verify.
No paid provider calls. Configuration serialization and bindings are unchanged.

## Risks and Compatibility

The visible Default label differs from the persisted codex token. The user guide
states that distinction; keymap command syntax and stored configuration remain
compatible. Skill upgrades affect developer workflows, so manifest and lock pins
are checked together. Reverting the label and pins requires no data migration.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-default-keymap-label | Compatible shipped presentation correction; fresh 0.7.3 from published 0.7.2. |

## Memory Impact

Status: none
Rationale: Maintenance pins and a display correction add no architectural or
workload decision beyond the bounded delivery recorded in this spec.

## Review and Evidence

Prospective independent review approved the fresh patch reservation and established
single-author ledger fallback on 2026-10-28. The runtime reported an uninitialized
board; no board or peer ownership was recreated. Public development manifest
confirmed 0.7.2 at 91a37883ecdbecc52158629777819839bac4080c.

Manifest/lock comparison passed for all 20 direct skills; the lock contains 21
entries including the transitive design-domain-model dependency. Existing startup
and footer tests cover AC-1, the Legacy label regression and existing override
checks cover AC-2. Native task test:interactive passed, including the SIGINT
subprocess test that timed out inside the sandbox. task docs:check and
versions:check passed. Independent exact review of e118c9e approved the candidate
subject to gates. Final independent exact review approved 3493e19 after the
compatibility section and focused-result reconciliation. Frozen task verify
passed all static checks and 2,138 tests. Hosted CI 40653386289 passed Linux
validation (including coverage, release qualification and terminal/process smokes)
and macOS tests/qualification on the PR candidate. Windows is excluded from this
repository's development-target CI and is not claimed passed.

## Integration Evidence

Revision: 52ce663c8c0079faf26611781932a3fc9a13120c
Outcome: passed

PR [#81](https://github.com/skills-yaml/nib/pull/81) merged into shared
`development` at 52ce663c8c0079faf26611781932a3fc9a13120c on
2026-10-28T08:33:44Z, pinned to independently reviewed head
3493e195ef4dec2d45c2d99968e28e5a54655ed8. The merge tree matches that head;
the local checkout fast-forwarded to the confirmed remote merge. This moves T092
to test and applies no second version bump. Main delivery and 0.7.3 publication
remain separate; neither is inferred here. The integration-record revision receives
fresh native documentation/version checks and full task verify before handoff.
