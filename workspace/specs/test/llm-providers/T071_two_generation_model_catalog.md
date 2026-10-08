# T071: Two-Generation Catalog and OpenRouter Mistral

State: test
Primary Feature: llm-providers

Status rationale: Confirmed shared development integration at `81c37a1908ec1032809e97cdf0b0b39d5253603b`
was re-read after the successful synchronization push on 2026-10-08T17:02:18Z.
Independent exact-candidate review and task verify/docs:check/check:all-targets
passed. Shared release is minor 0.4.0; T073 supersedes earlier retention
contracts. Main delivery, publication and T023 live qualification remain
separate.

Related: [T070](T070_three_generation_model_catalog.md),
[T069](T069_current_model_catalog_refresh.md) and
[T023](../../development/llm-providers/T023_live_llm_provider_model_integration_qualification.md).

## Problem and Scope

The user now prefers the latest two major generations per existing provider
family. This supersedes T070's three-generation policy while preserving its
source inspection and local validation history.
The additional Mistral request expands OpenRouter suggestions, not its protected
paid qualification allowlist. Remove GPT 4 suggestions from
OpenAI and OpenRouter, retaining GPT 6/5. Other families already have two or fewer
available major generations and retain their current suggestions: Claude 5/4,
Gemini 3/2, Grok 4, Muse Spark 1 and DeepSeek V4/V3. Point releases remain within
one major generation. Also add curated Mistral text/tool models to OpenRouter,
using exact public IDs and the latest two major generations per Mistral family.
No direct Mistral provider or new API implementation is introduced.

## Proposed Behavior

Remove `gpt-4.1`, `gpt-4.1-mini`, `gpt-4o` and `gpt-4o-mini` and their exact
OpenRouter counterparts from bundled suggestions only. Preserve defaults, the
remaining relative order, explicit selected models and configured replacement
lists. A configured GPT 4 selection remains usable and visible: this advisory
catalog is not an execution allowlist. Preserve T069's adapter repair and T023's
protected qualification fixtures unchanged. Add the following exact OpenRouter
IDs, keeping existing suggestions first:

```text
mistralai/mistral-large-4-0
mistralai/mistral-large-2512
mistralai/mistral-medium-3-5
mistralai/mistral-medium-3.1
mistralai/mistral-medium-3
mistralai/mistral-small-2603
mistralai/mistral-small-3.2-24b-instruct
mistralai/mistral-small-3.1-24b-instruct
mistralai/codestral-2508
mistralai/devstral-2512
mistralai/ministral-14b-2512
mistralai/ministral-8b-2512
mistralai/ministral-3b-2512
```

No paid or credentialed requests.

## Affected Areas

Bundled model TOML, registry and configuration tests, offline qualification
accounting fixture, user guide, spec catalog, shared release membership and
memory. Workload, persistence, delegation, adapters, endpoints, governed
instructions and historical spec records remain unchanged.

## Implementation Plan

1. Recheck the shared applied 0.3.2 reservation and add T071 membership without
   another bump; record the revised contract before implementation.
2. Remove the eight suggestions and adjust generation/configuration tests and
   offline budgets to the reduced direct-provider denominator.
3. Verify Mistral family generations and exact text/tool IDs using first-party
   documentation and OpenRouter public metadata; append the bounded selection.
4. Update current documentation and append the superseding durable preference.
5. Run focused Task checks, obtain independent exact-candidate review, freeze
   the candidate and complete the required aggregate gates.

## Acceptance Criteria

- [x] AC-1: The catalog contains 108 entries: OpenAI 11, Anthropic 13, Google 10,
  Grok 7, OpenRouter 63, Meta 3 and Mock 1. Only the eight GPT 4 suggestions are
  removed and the thirteen source-verified Mistral suggestions added. Defaults
  and remaining order are preserved.
- [x] AC-2: Bundled pickers omit GPT 4 suggestions while an explicitly selected
  GPT 4 model remains visible. Arbitrary custom selections and configured
  replacement/empty lists retain existing behavior.
- [x] AC-3: The offline full-matrix fixture accounts for 107 network entries,
  69 transport profiles, 276 requests, 828 maximum attempts and 17,664 output
  tokens. Mistral additions increase accounting entries only because OpenRouter
  execution
  remains separately allowlisted. Production qualification allowlists and budgets
  are unchanged.
- [x] AC-4: Current documentation, version membership and memory reflect two
  major generations; independent review and required gates pass.

## Validation Gates

Task-owned focused registry/configuration cases, `task test:llm-live:offline`,
`task docs:check` and `task versions:check` during iteration. Final frozen
candidate: `task verify`, `task docs:check`, `task check:all-targets` and
`git diff --check`. Serial Cargo access; no paid calls. Local gates do not
establish live qualification or branch delivery.

## Risks and Rollback

Picker removal must not become an execution restriction or rewrite configuration.
Keep free-form selection and explicit overrides. Revert only this catalog-policy
change to restore three-generation suggestions; preserve T069's repair and the
applied shared version. T070's public-source dates and availability restrictions
remain the evidence for the retained subset. Fresh public Mistral and OpenRouter
sources establish only catalog presence, not account entitlement or qualification.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-catalog-refresh | Compatible curated-suggestion policy revision, explicitly sharing T069/T070's unreleased, already-applied 0.3.2 patch from 0.3.1; atomic reservation rechecked before implementation, no second bump. Superseded at combined synchronization by the shared minor 0.4.0 target with T080; this compatible member remains patch impact and applies no additional bump. |

## Memory Impact

Status: updated
Rationale: Appended the user's superseding two-major-generation preference to
[workspace/agents/memory/preferences.md](../../../agents/memory/preferences.md)
and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md).

## Source Evidence

Retained non-Mistral models reuse T070's 2026-10-07 public-source inspection.
Fresh sources inspected on 2026-10-07 for Mistral:

- [Mistral model overview](https://docs.mistral.ai/models),
  [Large 3](https://docs.mistral.ai/models/mistral-large-3-25-12),
  [Small 4](https://docs.mistral.ai/models/mistral-small-4-0-26-03),
  [Devstral 2](https://docs.mistral.ai/models/devstral-2-25-12) and
  [Small 3.1 lifecycle](https://docs.mistral.ai/models/mistral-small-3-1-25-03)
  identify Large 4/3,
  Small 4/3, Medium 3, Ministral 3, Devstral 2 and Codestral's dated version.
- [Codestral capabilities](https://docs.mistral.ai/models/codestral-25-08)
  explicitly support Chat Completions and function calling.
- [OpenRouter public models](https://openrouter.ai/api/v1/models) list all thirteen
  exact IDs with text output and the `tools` parameter. Available router routes
  can outlast first-party deprecation or retirement; router membership is checked
  independently. Small 3.1 is retired on the direct Mistral API but remains
  listed with tools on OpenRouter; no direct Mistral availability is implied.

Large 4 launched October 6, 2026; Large 2 is outside the latest two-generation
window. Small 4/3 remain in-window. Medium 3.5/3.1/3 are point versions of
generation 3. Devstral 2 and Ministral 3 are separate families; Codestral's
`2508` is a dated release, not a fabricated major generation. No older Codestral
route is present in the inspected catalog. Batch variants, legacy Large 2 and
unselected specialty lines are excluded from this bounded curated addition.

## Local Implementation Evidence

The catalog now contains 108 entries, with 11 OpenAI and 63 OpenRouter entries.
The eight GPT 4 suggestions are removed; thirteen Mistral IDs are appended.
All 63 OpenRouter entries were checked against fresh public metadata for text
output and tool support. Focused and final aggregate gates pass.

Task-owned focused checks pass: 7 registry tests, 48 configuration tests and all
71 offline live-harness tests, with paid qualification ignored. The new selection
regression proves GPT 4 remains visible when explicitly configured despite its
omission from bundled suggestions. Registry tests check the eight omissions and
all thirteen Mistral additions. The offline fixture verifies exact accounting and
output/request ceilings. `task docs:check`, `task versions:check`, `task check`
and `git diff --check` pass. Protected qualification fixtures are unchanged.
Independent two-stage exact-candidate review reports no findings; final frozen
aggregate gates pass. This supports AC-1 through AC-3.

Frozen candidate fingerprint:
`4d343ea9af2d7b9d48660e5e49d37dfab92f0d5702a6570ad01ce9bae8e16ff9`.
All 27 manifest inputs remained unchanged throughout successful `task verify`,
`task docs:check` and `task check:all-targets`. The full suite passes 1,349 library
tests, 106 CLI tests, all 71 offline qualification cases and the remaining
integration/governance targets. Paid qualification remains ignored.
`git diff --check` passes. These results support AC-4 and all acceptance criteria.

Only this spec and its catalog rationale change during result reconciliation.
Renew independent exact-candidate review and the affected Workspace, documentation
and static modules. Native hash comparison must prove source/runtime inputs
unchanged before reusing their complete-suite evidence. Shared integration, main
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
