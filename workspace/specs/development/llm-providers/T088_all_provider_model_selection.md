# T088: Select Models Across Configured Providers

State: development
Primary Feature: llm-providers

## Scope

User requests selecting and using models from every configured provider via /model.
Follow-up to T024. Share structured provider/model choices between plain and TUI selectors.
Preserve exact unqualified IDs for the active provider, offline catalogs, overrides,
credentials and transport settings. No live discovery or workload migration.

## Acceptance Criteria

- [ ] AC-1: Both selectors list every configured provider’s effective suggestions,
  including selected custom IDs and explicit empty overrides, deterministically.
- [ ] AC-2: Selecting a provider/model choice persists both provider and model;
  identical IDs on different providers remain distinct. Existing factory resolution
  uses that active provider for the next run.
- [ ] AC-3: Unqualified exact IDs and single-provider behavior remain compatible;
  missing providers and empty model selections fail without mutation.
- [ ] AC-4: Regression tests, independent review and canonical gates pass.

## Affected Areas and Plan

Shared interactive selection/reducer, plain selector labels, regression tests,
user guide, spec catalog and release reservation. Build structured choices, persist
provider/model in one config update, test selection and failure paths, review and
run final gates. No execution-loop, delegation or external-system changes.

## Validation Gates

`task test:interactive`, `task versions:check`, `task docs:check`, `task verify`.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-all-provider-models | Public ModelSelection.available changes to structured choices; fresh 0.7.0 reserved from published 0.6.0 and applied once at development start. |

## Memory Impact

Status: updated
Rationale: The explicit all-provider selection preference is recorded in
[preferences](../../../agents/memory/preferences.md) and the
[changelog](../../../agents/memory/changelog.md).

## Delivery

No integration, main merge or publication yet. Missing peer board is preserved;
existing worktrees and primary checkout remain untouched. Dedicated detached worktree
based on published development 89156f6. No coordination-board recovery attempted.
