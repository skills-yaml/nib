# T088: Select Models Across Configured Providers

State: test
Primary Feature: llm-providers

## Scope

User requests selecting and using models from every configured provider via /model.
Follow-up to T024. Share structured provider/model choices between plain and TUI selectors.
Preserve exact unqualified IDs for the active provider, offline catalogs, overrides,
credentials and transport settings. No live discovery or workload migration.

## Acceptance Criteria

- [x] AC-1: Both selectors list every configured provider’s effective suggestions,
  including selected custom IDs and explicit empty overrides, deterministically.
- [x] AC-2: Selecting a provider/model choice persists both provider and model;
  identical IDs on different providers remain distinct. Existing factory resolution
  uses that active provider for the next run.
- [x] AC-3: Unqualified exact IDs and single-provider behavior remain compatible;
  missing providers and empty model selections fail without mutation.
- [x] AC-4: Regression tests, independent review and canonical gates pass.

## Affected Areas

Shared interactive selection/reducer, plain selector labels, regression tests,
user guide, spec catalog and release reservation. Build structured choices, persist
provider/model in one config update, test selection and failure paths, review and
run final gates. No execution-loop, delegation or external-system changes.

## Implementation Plan

1. Enumerate effective models for all configured providers into structured choices.
2. Carry provider identity through both selectors and persist selection atomically.
3. Verify duplicate/custom IDs, fallback, layered settings and both UI consumers.
4. Reconcile documentation and memory, independently review and run native gates.

## Risks and Rollback

The exported selection type changes, requiring the reserved minor release. Model
suggestions remain advisory: a provider may reject a listed model at runtime. The
existing error handling applies. Configuration layering must retain credentials
in user settings; regression coverage verifies that boundary. Rollback the code
change to restore the previous picker; persisted provider/model selections remain
valid configuration with no data migration.

## Validation Gates

`task test:interactive`, `task versions:check`, `task docs:check`, `task verify`.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-all-provider-models | Public ModelSelection.available changes to structured choices; fresh 0.7.0 reserved from published 0.6.0 and applied once at development start. |

## Memory Impact

Status: updated
Rationale: The explicit all-provider selection preference is recorded in
[workspace/agents/memory/preferences.md](../../../agents/memory/preferences.md) and the
[workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md).

## Delivery

Confirmed shared development merge at
`76ed1bffe49769965a7ced6471aa884437e1437e` via
[PR76](https://github.com/skills-yaml/nib/pull/76) on 2026-10-25T13:21:55Z,
pinned to independently reviewed head `0d06470cdf23180d327ebd6e68c67d61ee6713b7`.
The remote development ref was re-read at that merge, and Git confirms its tree
is identical to the qualified head. T088 enters test; main delivery and publication
are separate and remain unverified.

The missing peer board is preserved; existing worktrees and the primary checkout
remain untouched. The detached task worktree initially used published development
89156f6, then rebased onto shared development f8861c5 to retain configuration layers.
No coordination-board recovery was attempted.


## Review and Criterion Evidence

Independent spec-compliance and code-quality review approved the full source
candidate (diff from f8861c5 SHA-256
`f5c9c8fad2d42a9699b82fd8c91ace924ee06a6c5ce503cf34ba03796ba27cb6`).
Findings about opaque model IDs, implicit Mock/configured providers and minor
version classification were fixed and accepted by the reviewer.

AC-1: `all_providers_are_selectable_and_selection_preserves_provider_settings`
and `bundled_models_and_empty_overrides_are_provider_specific` pass. AC-2:
`chat_model_picker_switches_to_another_configured_provider`,
`model_picker_preserves_provider_identity_for_duplicate_ids` and the provider
settings/factory diagnostics test pass. AC-3:
`opaque_exact_ids_and_implicit_mock_remain_usable` and
`user_level_provider_selection_writes_only_project_preferences` pass; the latter
also verifies the user file is byte-for-byte unchanged and project preferences
contain neither credentials nor endpoints. Existing exact-ID tests pass.

AC-4: `task test:interactive`, `task docs:check` (including workspace and version
validation), and complete `task verify` passed on qualified head `0d06470`.
The full suite passed 1,659 library tests, 115 CLI tests, every integration suite
and strict Clippy/formatting checks. Hosted
[CI40194451812](https://github.com/skills-yaml/nib/actions/runs/40194451812)
passed Linux validation/coverage/release qualification/native terminal smokes and
macOS tests/release qualification/native terminal smokes. Windows was skipped by
the repository's development-branch workflow, not claimed qualified here.

The records-only reconciliation receives independent review and fresh final
`task docs:check` and `task verify`; those results are reported at handoff without
rewriting the frozen records again. Test commands use one
Cargo build job and disabled dev/test debug symbols to fit local memory; the
initial default-debug lib-test compile was killed by the host. This changes no
repository build policy or test selection. No paid provider requests are used.
