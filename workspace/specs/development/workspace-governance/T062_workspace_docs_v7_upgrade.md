# T062: Workspace Docs 7 Upgrade

**Status:** Development

State: development
Primary Feature: workspace-governance

## Scope

Upgrade this repository from workspace-docs@1.2.0 to workspace-docs@7.0.0.
The user approved the assessed migration map and necessary manual-policy,
runtime-path, validator, and release-filter reconciliation on 2026-10-02.
Preserve all pre-existing working-tree changes and all historical done states.
Local implementation does not establish shared integration or main merge.

## Confirmed Project Choices

Retain the single Rust CLI/library layout in Cargo.toml and src/, the shared
plain/TUI presentation, JSON session persistence, Task, and GitHub Actions.
No database, web frontend, infrastructure framework, or application refactor
is introduced. Existing Rust, Task, CI, permissions, and ecosystem guidance
remains authoritative after path and workflow reconciliation.
The shared test branch is development; the completion branch is main.

## Explicit Migration Map

| Source | Destination | Decision |
| --- | --- | --- |
| agents/memory/ | workspace/agents/memory/ | Preserve stable entries and add the migration decision. |
| docs/tech/ except architecture.md | workspace/instructions/tech/ | Retain project guidance; reconcile paths and v7 workflow. |
| docs/tech/architecture.md | workspace/docs/architecture/architecture.md | Preserve architecture reference and update links. |
| docs/standards/workspace-docs/README.md | workspace/instructions/standards/workspace-docs/ADOPTION.md | Preserve the old pin as historical provenance; remove machine-local source dependency. |
| Bundled complete standard | workspace/instructions/standards/workspace-docs/ | Copy regular files unchanged; recreate contained default/latest symlinks to v7.0.0. |
| docs/specs/{backlog,development,done}/ | workspace/specs/<state>/<primary-feature>/ | Categorize every spec and plan; preserve states and historical acceptance. |
| docs/specs/foundation/ | workspace/specs/legacy/foundation/ | Preserve product foundation and historical contract. |
| docs/specs/README.md | workspace/specs/README.md | Preserve audit evidence; add feature definitions and canonical catalog. |
| docs/user/guide.md | workspace/docs/user/guide.md | Retain guide; update runtime help discovery with legacy compatibility. |
| docs/projects/nib/inventory.md | workspace/docs/work/inventory.md | Reconcile current inventory while retaining historical evidence. |
| Generated AGENTS.md block | Root AGENTS.md | Use exact v7 template; reconcile approved conflicting manual paths/lifecycle only. |
| Missing DESIGN.md and reserved domains | Root DESIGN.md and workspace/ | Document existing UI contracts and reserved purposes, without invented product facts. |
| Runtime discovery, tests, Task, release filters | Existing source paths | Add new paths while retaining legacy runtime support; preserve release selection semantics. |

The full per-file map is recorded in [migration map](../../../docs/work/workspace-v7-migration-map.json).

## Acceptance Criteria

- [x] AC-1: The complete standard and required v7 structure exist with one active canonical location.
- [x] AC-2: Every non-legacy spec has one feature, state, catalog row, version impact, and valid memory impact; historical done evidence is preserved without fabricated releases.
- [x] AC-3: Project governance documents v7 review, blocked recovery, versioning, development integration, main completion, and separate publication.
- [x] AC-4: Runtime context and help discover migrated documents while retaining bounded legacy-path support.
- [x] AC-5: Native deterministic gates reject malformed catalog, memory, blocked, version, and coordination records; release path filtering preserves documentation-only exclusions.
- [x] AC-6: Independent exact-candidate review resolves all findings before task verify and applicable final gates pass.

## Affected Areas

AGENTS.md, DESIGN.md, README.md, workspace documentation/instructions/specs/memory,
Taskfile.yml, tests/docs_integrity.rs and native governance validators/tests,
src/context/{project_docs,help}.rs and regression tests, .github/workflows/release.yml,
and its installer contract test. Pending T059/T060 runtime changes are preserved. Current-stable gate repairs
are bounded to the existing async-trait 0.1 dependency, equivalent atomic update
method names, and a Task entry for the pinned dependency update.

## Implementation Plan

1. Record the pre-migration tree, source/destination map, and historical boundary.
2. Install the complete package, move each mapped area, and rewrite relative links.
3. Reconcile approved policy, project topology, catalog, memory and version records.
4. Add deterministic native validators with positive/negative fixtures and runtime compatibility regressions.
5. Run affected Task modules, reconcile knowable acceptance and memory, obtain independent review, freeze the candidate, then run final gates.
6. Keep this spec in development until confirmed shared integration; main merge is required for done under v7.

## Validation Gates

task docs:check, task versions:check, task test:workspace, task test:agent-context,
task test:installers, task test:task-contract, task verify, and git diff --check
for project-authored changes. Imported released standard files retain their
source bytes and are checked by the native SHA-256 structure gate; see the
preserved-source whitespace exception in ADOPTION.md.
No live-provider qualification or paid inference is authorized by this upgrade.

## Risks and Rollback

Relative links, runtime discovery and release filters depend on the old paths.
Preserve bounded traversal, no-link security and legacy compatibility; exercise
both paths and update filters without altering publication authority. Never
overwrite occupied destinations. Restore only migration changes on rollback,
preserving pending user work and historical records. Keep released packages
unchanged. The bundled migration guide contains stale v5 default wording;
the actual default/latest pointers, v7 manifest and v7 SDLC take precedence,
and an adoption note records this conflict without rewriting source packages.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-next | Adds compatible Workspace documentation discovery in the same next development release. |

## Memory Impact

Status: updated
Rationale: Accepted contract and adopted v7 governance recorded in workspace/agents/memory/decisions.md and workspace/agents/memory/changelog.md; implementation delivery evidence remains separate.

## Local Acceptance Evidence (2026-10-02)

AC-1: Native structure and source-package SHA-256 checks pass; legacy canonical
locations are removed only after preserving their content. The complete per-file
map records 95 moves; old whitespace-only .gitkeep markers are removed.
AC-2: Native catalog, memory and version checks pass for 70 historical done,
three development and two backlog specs, plus the single linked plan companion.
AC-3: Root policy uses the exact v7 generated template; manual policy changes
are limited to approved paths/lifecycle and removal of machine-local references.
The project SDLC records development integration, main completion and separate
publication. The source-guide conflict is documented in ADOPTION.md.

Iterative task check passed strict static checks. Validator fixtures pass all
seven tests. Focused gates, independent candidate review and complete local
verification are reconciled below. No integration, main merge or publication
is claimed.

## Independent Review Findings

The initial exact-candidate review found that moved architecture documentation
was missing from runtime discovery. Implemented its bounded no-link root and regression
coverage. Load only the pinned standard's live SDLC/process/memory/versioning
policies, excluding scaffolding templates so architecture and Task guidance
remain complete within the unchanged 128 KiB total bound.

The review also identified release-filter wording drift. Preserve the original
docs-only behavior for moved technical policies by excluding workspace/instructions/**
from Release Artifacts, with a contract regression. Both findings are implemented
and independently confirmed resolved. Root AGENTS.md, Task,
application and version-ledger changes remain eligible for release selection.
Renew independent review and run affected gates before final verification.

## Focused Validation and Review Resolution

All affected Task modules passed after the review fixes: task docs:check (five
integrity tests plus every native module), task test:workspace (seven positive/
negative fixture tests), task test:agent-context (91 context, 79 loop and two build
metadata tests), task test:installers (42 tests), and task test:task-contract (two
composition tests). No paid or credentialed test was run.

AC-4: Runtime fixtures prove current/legacy discovery, selected live standard
policies, no-link and path-traversal rejection, and complete architecture/Task
references within the unchanged aggregate bound.
AC-5: Validator fixtures cover malformed catalogs, duplicate rows, blocked resume
metadata, missing/invalid memory, release collisions/arithmetic/native drift,
unapplied integration versions and canonical coordination records. Installer
fixtures confirm documentation-only release exclusions and retained publication
controls.

The independent reviewer approved source candidate c30222112d417bbc296808e9c083542bab004a33
after confirming both findings resolved, with no additional actionable defects.
Supplemental review approved the acceptance-record candidate
2631e5fad75177edaede78313dc2cf9df5c2d2bf before complete local verification.

## Complete Local Verification and Record Reconciliation

`task verify` passed on the independently reviewed, frozen candidate
2631e5fad75177edaede78313dc2cf9df5c2d2bf. Static checks, all 1,256 library
tests, 93 CLI tests, integration suites and doctests passed. AC-6 is satisfied
for local implementation; the spec remains in development pending actual
shared integration and verified main merge.

The result-record update changes only this spec and ADOPTION.md. Imported
standard contents, runtime instructions, application sources, Cargo inputs,
Task definitions, workflows and fixtures remain unchanged. Renew independent
review and affected native governance/documentation verification for these
tracked records before handoff; do not infer freshness from their filenames
alone. Full verification evidence applies only to unchanged native inputs.

## Delivery Resumption (2026-10-05)

The original worktree was preserved in prerequisite commit
`04aef37bc8a0774dcc35da0e2447274c9f7362f8` and migration commit
`332c678d30d3a8700e840b39d5668393098a25dc`, based on development
`79a80c0759a7667b58bb1822bb9d2631df624c4b`. The prerequisite captures
already-completed local T059/T060 code so the migrated history, documentation
and gate commands remain consistent; it does not implement T061.

Independent reviewer t062-review approved that exact combined candidate for
spec compliance and technical/security quality with no findings. The only
difference from the previously reviewed source tree was migration result records.

Current production and development release manifests both report 0.1.0. The
shared atomic reservation confirms nib-next at 0.2.0. This delivery applies it
once to the native manifest, lockfile and project skill manifest before
integration artifacts, and reconciles the ledger to applied. Publication remains
a separate event. Version, record and policy deltas require renewed review and
fresh final gates. Shared integration and main merge are not yet claimed.

## Current-Stable Gate Repair Scope (2026-10-05)

Fresh task check on Rust 1.99 failed because fetch_update is deprecated in
favor of try_update, and async-trait 0.1.89 generates redundant must_use
attributes now rejected by Clippy. The compatibility repair updates the
existing async-trait dependency to released 0.1.92, whose macro removes that
redundant attribute, and renames five atomic calls without changing their
closures or memory ordering. No lint is disabled and no runtime/public trait
contract is redesigned. The Taskfile owns the reproducible locked dependency
update. Independent review and task verify cover the repaired exact candidate.

Focused migration modules passed before this repair: docs 5 tests, native
fixtures 7, context 91, agent loop 79, build metadata 2, installers 42, and
Task contracts 2. Those results are iteration evidence; dependency changes
require fresh final static and full-suite verification.

## Renewed Review and Frozen Handoff

Independent reviewer t062-review approved version/record candidate
`7abf4f6bf7bf1c75d2665e9b02add09440ae5088` and current-stable gate repair
`8dde598a28f494f92a3f0625abf2357b2844c152`, with no findings. The repaired
iterative task check passed all native governance checks, formatting and
warning-denying all-target/all-feature Clippy. The final handoff records
are reviewed before freezing; task verify then runs on that frozen revision.
No shared integration, main merge or paid provider qualification is yet claimed.

## Main-Promotion Security Follow-up

Independent combined-main review found a T058 read-only Git-status helper
execution path outside isolation. Linked [T063](../tools-sandbox/T063_isolated_read_only_git_status.md)
resolves that finding before promotion, preserving T058 history and the applied
shared version. Final verification is renewed after this repair; the interrupted
6a2e593 run is not completion evidence.
