# T070: Three-Generation Model Catalog

State: test
Primary Feature: llm-providers

Status rationale: Confirmed shared development integration at `81c37a1908ec1032809e97cdf0b0b39d5253603b`
was re-read after the successful synchronization push on 2026-10-08T17:02:18Z.
Independent exact-candidate review and task verify/docs:check/check:all-targets
passed. Shared release is minor 0.4.0; T073 supersedes earlier retention
contracts. Main delivery, publication and T023 live qualification remain
separate.

Related: [T069](T069_current_model_catalog_refresh.md),
[T024](../../done/llm-providers/T024_configurable_provider_model_catalog_and_curated_defaults.md)
and [T023](../../development/llm-providers/T023_live_llm_provider_model_integration_qualification.md).

## Problem and Scope

The refreshed picker mostly lists the newest models. Retain useful available
text/tool models from the latest three major generations of each existing
provider family. Point releases belong to one major generation: GPT 6/5/4,
Claude 5/4/3, Gemini 3/2/1 and Grok 4/3/2. A provider with fewer available
generations advertises fewer; never resurrect retired or redirected old IDs to
fill a quota. OpenRouter applies retention to its existing upstream families.

## Proposed Behavior

Expand the advisory bundled lists using exact publicly documented IDs, with
existing entries first and available alternatives ordered newest to oldest within each family. Keep all
T069 defaults and existing selections/override behavior. Prefer canonical active
text/tool aliases over duplicate dated snapshots, batch variants, specialized
audio/image models, and models requiring unsupported request shapes. This is a
curated catalog, not an exhaustive provider inventory or runtime discovery.
Public documentation establishes presence, not account entitlement or paid
qualification. Document account restrictions and unavailable generations.

No adapter, endpoint, credential, workload, persistence or delegation changes.
Preserve T069's Anthropic repair and T023's protected qualification allowlist and
selected matrix. No paid requests or user configuration migration.

## Affected Areas

Bundled model TOML, configuration and registry regression tests, offline full
qualification planner fixture, current user guide, this spec and status catalog,
release membership, and project memory. Governed instructions are out of scope.

## Implementation Plan

1. Inspect first-party model and retirement documentation and OpenRouter's public
   tool-capable text catalog; record exact IDs and generation limits.
2. Join T069's explicitly shared unreleased patch reservation, already applied
   at 0.3.2 from 0.3.1; add membership without applying a second bump.
3. Expand the curated lists and verify inherited picker ordering, selected custom
   models, override isolation and full-planner accounting against the new data.
4. Reconcile guide, source evidence and durable user retention preference.
5. Obtain independent exact-candidate review and run focused and full Task gates.

## Acceptance Criteria

- [x] AC-1: Available representative text/tool models span up to three major
  generations per existing family; exact IDs, availability exceptions and source
  dates are recorded. Current defaults and entries remain present.
- [x] AC-2: The picker inherits the expanded lists in order, keeps selected custom
  models visible, and respects configured replacement lists unchanged.
- [x] AC-3: The offline full-matrix fixture accounts for every direct catalog entry
  and only separately approved OpenRouter profiles with sufficient fixed budgets.
  Protected paid qualification IDs are unchanged.
- [x] AC-4: Documentation, shared version membership and memory reflect the change;
  independent review and required local gates pass without paid calls.

## Validation Gates

Task-owned focused registry/configuration cases, `task test:llm-live:offline`,
`task docs:check` and `task versions:check` during iteration. Final frozen
candidate: `task verify`, `task docs:check`, `task check:all-targets` and
`git diff --check`. Cargo runs serially. Local verification does not establish
T023 live qualification, shared integration or main delivery.

## Risks and Rollback

Older models can have restricted access or upcoming retirement. Source dates and
documented restrictions make that boundary explicit; free-form configuration
remains available. Expanded direct-provider lists increase a separately authorized
full qualification denominator; preserve fail-closed request budgets. Revert the
catalog extension to roll back; preserve the applied shared version and T069 fix.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-catalog-refresh | Compatible correction to curated catalog retention; explicitly shares T069's unreleased, already-applied 0.3.2 patch from 0.3.1. Atomic reservation rechecked before implementation; no second bump. Superseded at combined synchronization by the shared minor 0.4.0 target with T080; this compatible member remains patch impact and applies no additional bump. |

## Memory Impact

Status: updated
Rationale: Recorded the confirmed three-major-generation retention preference in
[workspace/agents/memory/preferences.md](../../../agents/memory/preferences.md)
and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md)
after source verification and catalog implementation.

## Source Evidence

Public sources inspected on 2026-10-07; no credential or generation requests.

- [OpenAI complete catalog](https://developers.openai.com/api/docs/models/all)
  and [deprecations](https://developers.openai.com/api/docs/deprecations): curated
  GPT 6/5/4 text/tool models retain three major generations; omit specialized,
  duplicate snapshot and shutdown variants. Responses remains the documented
  transport for reasoning tool use; existing configured modes remain explicit.
- [Anthropic lifecycle catalog](https://platform.claude.com/docs/en/about-claude/model-deprecations)
  and [Opus 4.5 alias](https://platform.claude.com/docs/en/models/opus-4-5/overview):
  5.x and 4.x remain available; every 3.x entry is retired. Preserve the existing
  dated Haiku entry and use canonical aliases for additions. Sonnet 4.5 is
  deprecated but available until November 30, 2026.
- [Google catalog](https://ai.google.dev/gemini-api/docs/models),
  [3.1 Flash-Lite](https://ai.google.dev/gemini-api/docs/models/gemini-3.1-flash-lite),
  and [deprecations](https://ai.google.dev/gemini-api/docs/deprecations/): add
  2.5 Pro/Flash/Flash-Lite, restricted to users who previously actively used them.
  Generation 1.5 and 2.0 text models are shut down; keep available 3.x models.
- [xAI pricing](https://docs.x.ai/developers/pricing),
  [reasoning alias](https://docs.x.ai/developers/models/grok-4.20-0309-reasoning),
  [non-reasoning alias](https://docs.x.ai/developers/models/grok-4.20-0309-non-reasoning)
  and [retirement mapping](https://docs.x.ai/developers/migration/may-15-retirement):
  Grok 4.7/4.6/4.5/4.3 and two distinct 4.20 variants are available; Build 0.1 is
  a separate coding line. Retired Grok 3 redirects to 4.3. Exclude multi-agent
  variants because their endpoint lacks nib's client-side tool and cap contract.
- [Meta hosted catalog](https://dev.meta.ai/docs/models): standard Muse Spark
  1.3/1.2/1.1 support text/tool Chat Completions and form one major generation.
  Omit Contributor variants and self-hosted model families; preserve 1.1 default.
- [OpenRouter public catalog](https://openrouter.ai/api/v1/models): all 54 curated
  exact IDs have text output and the `tools` parameter. GPT 6/5/4, Claude 5/4,
  Gemini 3/2, Grok 4 and DeepSeek V4/V3 remain represented. Router-only available
  Claude Opus 4.1 and Sonnet 4 are independent of direct Anthropic retirement.
  No V2/V1 DeepSeek tool model is present; R1 is a separate reasoning line,
  not a third V generation. No batch variants or new upstream providers.

## Local Implementation Evidence

The catalog expands from 25 to 103 entries: OpenAI 15, Anthropic 13, Google 10,
Grok 7, OpenRouter 54, Meta 3, Mock 1. All prior defaults and suggestions remain.
The offline synthetic full catalog contains 102 network entries; all four
historical approved OpenRouter IDs are now also present among suggestions.
Expected full accounting is 77 transport profiles, 308 logical requests,
924 maximum attempts and 19,712 maximum output tokens. These are offline fixture
budgets, not approval for a paid run. Final frozen aggregate gates pass.

Focused registry checks (7 cases) and configuration checks (47 cases) pass,
including inherited complete ordering, custom selection visibility, and explicit
replacement/empty override behavior. Public OpenRouter exact-ID metadata was
checked independently for all 54 entries. These support AC-1 and AC-2.

All 71 credential-free live-harness cases pass, with the paid entry point ignored.
The full dry-run fixture verifies exact 102-entry accounting, 77 profiles,
308 requests, 924 attempts and 19,712 tokens. Protected selected-model and router
allowlist files are unchanged. This supports AC-3. Independent spec-compliance
and quality exact-candidate review approved the frozen candidate without findings.

Final frozen candidate fingerprint:
`0a996643672fc5272318b8bd1226b0c5d78aea14e9a07a905af80befd260b99e`.
All 26 manifest inputs remained unchanged throughout `task verify`,
`task docs:check` and `task check:all-targets`, all successful. The full suite
includes 1,348 library tests, 106 CLI tests, all 71 offline qualification cases,
and the remaining integration and governance targets. Paid qualification remains
ignored. `git diff --check` passes. These results support AC-4 and every criterion.

Result reconciliation changes only this spec and its catalog rationale. Renew
independent exact-candidate review and the affected Workspace/documentation/static
modules for those records. Preserve source/runtime verification only after native
hash comparison proves their frozen inputs unchanged. No shared development
integration, main merge or publication is observed; this spec remains development.

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

## Shared Integration Evidence (2026-10-08)

Outcome: passed

The synchronization push advanced remote `development` from `045e0e2` to
`81c37a1908ec1032809e97cdf0b0b39d5253603b`. The remote ref was re-read at
2026-10-08T17:02:18Z and matched that exact reviewed combined revision.
Independent spec-compliance and correctness review approved the candidate
after the two documentation findings were fixed. Fresh serial `task verify`,
`task docs:check`, `task check:all-targets` and `git diff --check` passed
before pushing. Verification used debug-symbol-free build profiles after the
earlier filesystem-capacity failure; test assertions and coverage of the
complete non-ignored suite were preserved.

This observed shared integration establishes test state under minor 0.4.0.
T073 governs the final catalog selection; earlier retention proposals remain
preserved as superseded history. This record does not establish main delivery,
publication, paid qualification or completion of later T080 phases. The
subsequent lifecycle-record change renews independent review and affected
documentation, governance and static checks; runtime inputs remain unchanged.
