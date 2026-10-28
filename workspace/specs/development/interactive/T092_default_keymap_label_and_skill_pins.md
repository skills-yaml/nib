# T092: Default Keymap Label and Workspace Skill Pins

State: development
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

- [ ] AC-1: Startup and contextual hints display Default for the Codex preset.
- [ ] AC-2: Legacy hints and configured bindings retain their behavior.
- [ ] AC-3: All 20 direct skill pins agree with the lockfile; its additional
  design-domain-model entry remains a transitive dependency.
- [ ] AC-4: Native mirrors agree on the single reserved patch and required gates pass.

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
checks cover AC-2. Focused and full gate results remain pending; no integration,
main delivery or publication is claimed.
