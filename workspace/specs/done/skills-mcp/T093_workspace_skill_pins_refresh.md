# T093: Workspace Skill Pins Refresh

**Status:** Done.
State: done
Primary Feature: skills-mcp

## Scope

Adopt the user-approved working tree changes refreshing direct workspace skill pins
in `skills.yaml` and `skills.lock.yaml` to their current releases. Follows up [T092](../../done/interactive/T092_default_keymap_label_and_skill_pins.md),
which previously integrated keymap presentation improvements while leaving the refreshed skill pin edits in the working tree.
Preserve skill configurations, outputs, registry sources, and transitive dependency boundaries.
No governed instruction changes.

## Acceptance Criteria

- [x] AC-1: Direct skill pins in `skills.yaml` are updated to match current available versions across 20 skills:
  - `workspace/address-review-feedback`: `v0.4.9`
  - `workspace/adopt-workspace-structure`: `v0.11.1`
  - `workspace/analyze-repository`: `v0.4.5`
  - `workspace/coordinate-multi-agent-development`: `v0.2.9`
  - `workspace/fix-bug`: `v0.4.9`
  - `workspace/fix-ci`: `v0.4.9`
  - `workspace/implement-spec`: `v0.5.5`
  - `workspace/maintain-project-docs`: `v0.3.9`
  - `workspace/monitor-ci`: `v0.4.9`
  - `workspace/plan-implementation`: `v0.5.5`
  - `workspace/review-changes`: `v0.5.5`
  - `workspace/review-security`: `v0.5.5`
  - `workspace/wk-adopt`: `v0.4.4`
  - `workspace/wk-analyze`: `v0.4.5`
  - `workspace/wk-deliver`: `v0.4.5`
  - `workspace/wk-document`: `v0.3.9`
  - `workspace/wk-plan`: `v0.4.5`
  - `workspace/wk-review`: `v0.4.5`
  - `workspace/wk-spec`: `v0.4.5`
  - `workspace/write-spec`: `v0.6.3`
- [x] AC-2: All direct skill pins in `skills.lock.yaml` match `skills.yaml`, and transitive dependency `workspace/design-domain-model@0.1.5` is locked.
- [x] AC-3: Workspace governance checks pass cleanly (`task workspace:check`).
- [x] AC-4: Native skill tests (`task test:skills`) and repository static checks (`task check`) pass without error.

## Affected Areas

`skills.yaml`, `skills.lock.yaml`, `workspace/specs/done/skills-mcp/T093_workspace_skill_pins_refresh.md`, `workspace/specs/README.md`, and `workspace/releases.json`.
Consumers are agent skill execution and workflow skills across supported agents (Codex, Claude, Cursor, Grok, Hermes).

## Implementation Plan

1. Verify `skills.yaml` and `skills.lock.yaml` pin parity and transitive dependencies.
2. Register T093 development spec in catalog (`workspace/specs/README.md`) and join shared `0.7.3` release in `workspace/releases.json`.
3. Validate workspace invariants with `task workspace:check`, `task docs:check`, and `task versions:check`.
4. Run static gates (`task check`) and skill suite (`task test:skills`).

## Validation Gates

- `task workspace:check`
- `task docs:check`
- `task versions:check`
- `task check`
- `task test:skills`

## Risks and Compatibility

Skill updates only affect developer agent automation workflows and instructions; the compiled nib runtime binary, data models, and persisted session formats remain unchanged.
Reverting pins requires only checking out previous manifest and lockfile versions without data migration.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-default-keymap-label | Workspace skill pins upgrade shares unreleased applied 0.7.3 patch reservation; no second bump applied. |

## Memory Impact

Status: none
Rationale: Updating external workspace developer skill pins introduces no changes to runtime architecture, workload models, or persistent agent state.

## Integration Evidence

Revision: 1dba2e76f5b338cb81a67ad84fb34991823ebca0
Outcome: passed

PR [#82](https://github.com/skills-yaml/nib/pull/82) merged into shared `development` at 1dba2e76f5b338cb81a67ad84fb34991823ebca0 on 2026-11-15 after independent CI qualification (Validate and macOS tests passed) and full local verification. This moves T093 to test under the unreleased applied 0.7.3 patch reservation. Main delivery and publication remain separate.

## Main Merge Evidence

Revision: dd577725a2af6a51fc972a0b56afd7798f4ef34c
Outcome: passed

Confirmed fast-forward delivery from `development` to shared `main` at revision `dd577725a2af6a51fc972a0b56afd7798f4ef34c` (`dd57772`) on 2026-11-20. All required platform tests and verification gates passed. Publication remains tracked separately.
