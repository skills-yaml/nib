# T072: Latest-Major-Generation Model Catalog

State: development
Primary Feature: llm-providers

Status rationale: On 2026-10-07 the user revised the catalog to the last major
generation. Implementation is independently reviewed and locally verified. Shared integration,
main delivery and publication require separate observed evidence.

Related: [T071](T071_two_generation_model_catalog.md),
[T070](T070_three_generation_model_catalog.md),
[T069](T069_current_model_catalog_refresh.md) and
[T023](T023_live_llm_provider_model_integration_qualification.md).

## Problem and Scope

The user now prefers one major generation within the existing curated model
families. This supersedes T071's two-generation contract while preserving its
source inspection and local verification history. Picker consumers receive the
latest available major generation: GPT 6, Claude 5, Gemini 3, Grok 4, Muse Spark 1
and DeepSeek V4. Mistral through OpenRouter retains Large 4, Small 4, Medium 3,
Ministral 3, Devstral 2 and the available Codestral 25.08 dated release. Grok Build
remains a separate coding line. Point releases remain one major generation.

## Proposed Behavior

Filter the existing catalog, removing 44 suggestions and adding no new IDs:
seven direct GPT 5 entries, seven direct Claude 4 entries, three direct Gemini 2
entries, and their router counterparts (seven GPT 5, nine Claude 4 and three
Gemini 2 entries), plus five DeepSeek V3 routes and three older Mistral routes.
The latter are `mistralai/mistral-large-2512`,
`mistralai/mistral-small-3.2-24b-instruct` and
`mistralai/mistral-small-3.1-24b-instruct`.

Preserve all seven defaults and the remaining relative order. Keep explicit
selected models and replacement/empty suggestion lists authoritative: bundled
removal must not restrict execution or rewrite configuration. Preserve T069's
Anthropic adapter repair, protected qualification fixtures, production planner
limits, version sources and unrelated user edits. No new provider, adapter,
endpoint, credential access, paid request or governed instruction change.

## Affected Areas

Bundled TOML, registry/configuration regression tests, offline qualification
accounting fixture, current user guide, spec status catalog, shared release
membership and durable preference memory. No workload, persistence, delegation,
transport or external-system mutation.

## Implementation Plan

1. Recheck the applied 0.3.2 shared reservation and register T072 without another
   bump; record this contract before implementation.
2. Filter the catalog and update focused generation, selected-model and exact
   qualification accounting tests. Preserve the synthetic approved GPT 5 router
   fixture entry even though it is no longer a picker suggestion.
3. Reconcile the current guide and append the superseding preference and memory
   changelog; preserve earlier spec/source/verification history.
4. Run focused Task gates, obtain independent two-stage exact-candidate review,
   freeze the candidate and complete all required final gates.

## Acceptance Criteria

- [x] AC-1: Exactly 64 entries remain: OpenAI 4, Anthropic 6, Google 7, Grok 7,
  OpenRouter 36, Meta 3 and Mock 1. Only the specified 44 older suggestions are
  removed; defaults, retained IDs and relative order are unchanged.
- [x] AC-2: Unconfigured pickers omit prior-generation suggestions, while
  explicitly selected older models remain visible once and usable. Custom
  selection and replacement/empty list behavior remains unchanged.
- [x] AC-3: The offline full-matrix fixture accounts for 64 network entries
  (63 suggestions plus its separately approved GPT 5 route), 45 transport
  profiles, 180 requests, 540 maximum attempts and 11,520 output tokens. Its
  four approved profiles and all protected production qualification fixtures
  and planner budgets are unchanged.
- [x] AC-4: Current documentation, release membership and durable memory reflect
  the latest-major-generation contract. Independent exact-candidate review and
  required Task gates pass.

## Validation Gates

Focused registry/configuration cases through the Task test interface;
`task test:llm-live:offline`, `task docs:check`, `task versions:check` and
`task check` during iteration. Final frozen candidate: `task verify`,
`task docs:check`, `task check:all-targets` and `git diff --check`.
Serialize Cargo with the existing bounded build settings. Each criterion needs
observable supporting evidence. Local checks do not establish authenticated
qualification, integration, main merge or publication.

## Source Evidence

Retained IDs are a subset of the source-verified T071 catalog inspected on
2026-10-07. Rechecked public model overviews on the same date:
[OpenAI](https://developers.openai.com/api/docs/models),
[Anthropic](https://platform.claude.com/docs/en/models/overview),
[Google](https://ai.google.dev/gemini-api/docs/models),
[xAI](https://docs.x.ai/developers/models),
[Meta](https://dev.meta.ai/docs/models),
[DeepSeek](https://api-docs.deepseek.com/quick_start/pricing/) and
[Mistral](https://docs.mistral.ai/models).
All 36 retained OpenRouter IDs were freshly checked against the
[public model catalog](https://openrouter.ai/api/v1/models) for text output and
the `tools` parameter. Router IDs use independently verified metadata rather than
direct API aliases. Catalog presence is distinct from account access and qualification.

## Risks and Rollback

The retention window is advisory. Do not mistake picker removal for execution
denial or expand qualification to every picker suggestion. Preserve point
releases within the retained major and dated Codestral naming. Restore only the
T071 catalog-policy inputs to roll back this revision; retain T069's adapter
repair, historical records and the shared applied version.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-catalog-refresh | Compatible curated-suggestion policy revision, sharing T069/T070/T071's unreleased applied 0.3.2 patch from 0.3.1; atomic reservation rechecked before implementation, no additional bump. Superseded at combined synchronization by the shared minor 0.4.0 target with T080; this compatible member remains patch impact and applies no additional bump. |

## Memory Impact

Status: updated
Rationale: Appended the superseding latest-major-generation preference to
[workspace/agents/memory/preferences.md](../../../agents/memory/preferences.md)
and the corresponding
[workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md) record.

## Local Evidence

All 44 specified suggestions are removed, leaving 64 entries. Native TOML
inspection proves all defaults and retained ordering unchanged. The 36 retained
OpenRouter routes are present with text output and tools in fresh public metadata.
This supports AC-1.

Focused Task tests pass: seven registry cases, 48 configuration cases and all
71 offline qualification cases, with paid qualification ignored. The selected-model
regression covers both removed GPT 5 and GPT 4 selections; existing custom and
replacement/empty list cases pass. The offline fixture exercises union with its
separately approved GPT 5 router route and checks the complete denominator and
ceilings. This supports AC-2 and AC-3. `task docs:check` and its five integrity
cases, including native version/memory governance, pass. All 18 prior adapter,
version, user-edit and historical manifest inputs remain byte-identical to T071.
Protected qualification fixtures are unchanged.

Independent spec-compliance and correctness review of exact candidate
`f2a88a9c7596b8704f563cea24f891422e7aefac8f72f76ec174b2376fb39d22`
reports no findings. All 28 manifest inputs remained unchanged throughout
successful frozen `task verify`, `task docs:check` and `task check:all-targets`.
The full suite passes 1,349 library tests, 106 CLI tests, all 71 offline
qualification cases and the remaining integration/governance targets. Paid
qualification remains ignored. `git diff --check` passes. The shared applied
0.3.2 reservation was rechecked at handoff without another bump. These results
support AC-4 and all acceptance criteria.

Only this spec and its catalog rationale change during result reconciliation.
Renew independent exact-candidate review and affected Workspace, documentation
and static gates. Native hash comparison must prove the other 26 reviewed inputs
unchanged before reusing their full-suite evidence. Shared integration, main
delivery and publication remain unobserved; the spec stays development.

## Shared Release Reconciliation (2026-10-08)

T080 phase 1 is already integrated into remote development at `045e0e2`.
The user-requested synchronization combines that work with the catalog and
Anthropic changes, retaining shared release `nib-catalog-refresh` at minor
0.4.0 from 0.3.1. The release ledger includes all five catalog specs and T080;
Cargo.toml, Cargo.lock and skills.yaml retain the same applied 0.4.0 target.
Earlier 0.3.2 statements record the superseded pre-integration reservation.
Combined exact review and native gates are pending; no catalog shared
integration, main delivery, publication or live-provider qualification is
inferred from this local reconciliation.
