# T069: Current Model Catalog Refresh

State: development
Primary Feature: llm-providers

Status rationale: User requested the catalog upgrade on 2026-10-06 and explicitly
included the Anthropic compatibility fix. Implementation, independent review
and local verification pass; shared development integration, main delivery and
publication require their own observed evidence.

Related: [T024](../../done/llm-providers/T024_configurable_provider_model_catalog_and_curated_defaults.md),
[T068](../../done/llm-providers/T068_anthropic_stream_continuation_preservation.md),
and [T023](T023_live_llm_provider_model_integration_qualification.md).

## Problem and Scope

The bundled picker still defaults to older models. Refresh the curated text/tool
catalog from current public provider sources, preserving the balanced OpenAI Sol
role, Anthropic Opus role, Google Flash role and Grok general-purpose role.
Anthropic Opus 5.5 requires thinking on every request, while nib currently sends
`thinking: disabled` on capped text requests. Remove that override for all
Anthropic models, preserving the explicit total output-token ceiling and normal
provider thinking defaults. Signed thinking also requires the unchanged preceding
conversation: retain every private native tool turn across successive tool batches
under cumulative continuation bounds. Do not infer capabilities from model names.

## Proposed Behavior

- OpenAI: default `gpt-6.1-sol`; suggestions also include `gpt-6-astra` and
  `gpt-6-luna`. New OpenAI authentication already selects Responses, which these
  models require for reasoning tool use. Existing configured transports remain
  explicit and existing actionable provider failures remain authoritative.
- Anthropic: default `claude-opus-5-5`; also `claude-fable-5-1`,
  `claude-sonnet-5-5`, and `claude-haiku-4-5-20251001`. Direct API IDs use hyphens;
  OpenRouter uses its separately verified dotted canonical IDs.
- Google: default `gemini-3.8-flash`; retain `gemini-3.7-flash`,
  `gemini-3.6-flash`, `gemini-3.5-flash`, `gemini-3.5-flash-lite` and
  `gemini-3.1-pro-preview` as documented alternatives.
- Grok: default and curated suggestion `grok-4.7`, supported by its documented
  legacy Chat Completions endpoint as well as Responses.
- OpenRouter: default `openai/gpt-6.1-sol`; also `openai/gpt-6-astra`,
  `openai/gpt-6-luna`, `anthropic/claude-opus-5.5`,
  `anthropic/claude-fable-5.1`, `anthropic/claude-sonnet-5.5`,
  `google/gemini-3.8-flash`, `x-ai/grok-4.7`, and
  `deepseek/deepseek-v4.1-flash`. Every listed entry has tools in the public
  OpenRouter supported parameters.
- Meta remains `muse-spark-1.1`; Mock remains `mock-model`.
- Record public-source inspection date 2026-10-06 for all entries. This date
  means catalog verification, not authenticated generation qualification.
- Existing explicit selected models and `models` overrides remain unchanged.
  The selected model remains visible even when absent from the refreshed list.
- Anthropic bounded requests omit a thinking override, leave `max_tokens`
  unchanged, and retain truncation/refusal failure boundaries and private native
  thinking preservation in complete and streaming tool continuations. Retain the
  full native assistant/tool-result history across batches, bounded by the existing
  256-item and 4 MiB limits including encoded tool-result wire strings. Preserve
  the original fallback-system-prompt choice across Required-to-Auto turns;
  generation options outside the signed prefix may change normally.

## Non-Goals

No credential access, paid generation, automatic config migration, prompt/effort
tuning, provider endpoint change, new schema or inferred model capabilities.
Do not modify governed instructions or historical done specs. T023's selected
matrix and protected OpenRouter allowlist retain their separately approved IDs;
their qualification is not bypassed by changing picker defaults. A canary follows
the new default only in a separately authorized run.

## Affected Areas

`src/llm/default_models.toml`, `src/llm/anthropic.rs`, registry/config tests,
`tests/llm_live_support/plan.rs` offline catalog fixture, current user guide,
lifecycle catalog, native version mirrors, release ledger,
work records and memory. Persistence, workload state, delegation and external
systems have no migration.

## Implementation Plan

1. Confirm provider sources, exact IDs and transport/thinking requirements.
2. Reserve patch 0.3.2 from published 0.3.1 through the atomic board; apply the
   bump once at development-start before implementation artifacts.
3. Reproduce the bounded Anthropic request defect with deterministic tests.
4. Refresh catalog data, remove the thinking override, reconcile config tests
   and update current documentation while preserving historical fixtures.
5. Run focused provider/config and documentation gates; reconcile durable memory.
6. Obtain independent exact-candidate spec-compliance and quality review, fix
   findings, then freeze and run complete native gates.

## Acceptance Criteria

- [x] AC-1: All seven providers have valid, source-dated curated defaults matching
  the IDs and ordering above; OpenRouter IDs and tool support are verified.
- [x] AC-2: Existing selected models and configured picker overrides preserve
  their values, order and free-form selection behavior after the refresh.
- [x] AC-3: Complete and streaming bounded Anthropic text requests omit the
  thinking override and preserve exact nonzero output ceilings. Unbounded and
  tool requests retain their existing provider-default behavior.
- [x] AC-4: Native thinking/signature/redacted blocks and every earlier native
  assistant/tool-result pair survive successive complete/stream tool batches
  privately with unchanged prefixes, including the inferred system-prompt presence
  across Required-to-Auto turns, and cumulative 256-item/4 MiB bounds.
  Truncation, refusal, malformed output and invalid ceilings cannot produce
  executable authority.
- [x] AC-5: Current documentation, versions, catalog and memory match the
  candidate; independent review and required Task gates pass without paid calls.

## Validation Gates

During iteration use Task-owned focused fixtures, `task test:llm-conformance`,
`task test:llm-live:offline`, `task docs:check` and `task versions:check`.
Final frozen candidate: `task verify`, `task docs:check`,
`task check:all-targets` and `git diff --check`. All Cargo operations are serial.
Local verification does not establish T023 live qualification or main delivery.

## Risks and Rollback

Models can change availability, costs and latency. Anthropic thinking shares the
total token ceiling and can exhaust small caps before text; preserve explicit
truncation outcomes rather than silently increasing budgets or retrying with
different semantics. Reverting the catalog/request edit restores prior behavior;
never decrement or reuse a published version. Long tool sequences can reach
cumulative native-history limits; fail without dropping the signed prefix or
executing partial tool authority. User configs are not rewritten.

## Source Evidence

Inspected on 2026-10-06:

- [OpenAI models](https://developers.openai.com/api/docs/models) and
  [GPT-6 migration](https://developers.openai.com/api/docs/guides/latest-model/gpt-6-astra.md#migration-quickstart).
  Official resolver returned Astra; preserve the existing balanced Sol default
  role using documented GPT-6.1 Sol rather than collapsing all roles to Astra.
- [Anthropic models](https://platform.claude.com/docs/en/models/overview) and
  [Opus 5.5 migration](https://platform.claude.com/docs/en/models/opus-5-5/migration-guide)
  and [preserved-thinking prefix contract](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#keeping-the-prefix-unchanged).
- [Google models](https://ai.google.dev/gemini-api/docs/models).
- [Grok models](https://docs.x.ai/developers/models) and
  [Chat Completions](https://docs.x.ai/developers/model-capabilities/legacy/chat-completions).
- [OpenRouter public model catalog](https://openrouter.ai/api/v1/models).
- [Meta Muse Spark 1.1](https://ai.meta.com/blog/introducing-muse-spark-meta-model-api/).
- Mock is the local deterministic adapter.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-catalog-refresh | Compatible refresh of existing curated model roles and repair of capped Anthropic request compatibility; atomic 0.3.2 from published 0.3.1, development-start. |

## Memory Impact

Status: updated
Rationale: Appended the public catalog refresh, exact-ID distinction, total token
ceiling, provider-controlled thinking boundary and accumulated private native
history and fallback-system-prompt contracts to [workspace/agents/memory/facts.md](../../../agents/memory/facts.md)
and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md).
No live qualification or delivery event is inferred.

## Local Implementation Evidence

The capped-text regression failed before the fix because the request contained
`thinking: disabled`. Focused Task fixtures then passed: seven registry cases,
47 configuration cases, 18 Anthropic unit cases and eight bounded/private native
continuation cases. Initial fixtures support AC-1 through AC-4, including exact cap preservation
for complete/stream, zero-cap rejection before I/O, token exhaustion/refusal and
private native continuation under the refreshed default. OpenRouter catalog
inspection confirmed every curated ID with tool support.

Independent review identified a required signed-prefix repair: latest-turn-only
continuations dropped earlier native tool batches. The revised candidate retains
accumulated private history and adds three-request complete/stream prefix and
cumulative item/byte-bound regressions. All eleven bounded/private-history
fixtures pass, including both complete and streaming repeated-tool paths.
Independent exact-candidate review approved the accumulated-history repair.
Subsequent inspection identified fallback-system-prompt insertion when Required
qualification requests continue with Auto. The candidate now preserves the
original fallback choice; an added three-request complete/stream regression
covers that path. All twelve bounded/private-history fixtures pass, including
Required-to-Auto complete/stream continuation. All 38 conformance cases,
`task docs:check` and `task check` pass. Independent exact-candidate review
approved the final prefix repair with no findings. Final frozen `task verify`
and applicable gates pass as recorded below.

Implementation, independent review and durable memory are reconciled for AC-5.
Final frozen native gates pass and support AC-5 as recorded below. This
development spec records no delivery transition. The offline full-matrix fixture now includes both current picker suggestions
and independently approved historical OpenRouter IDs. Its exact denominator
is 28 catalog entries, 24 profiles, 96 requests, 288 maximum attempts and 6,144
output tokens; all 71 offline qualification tests pass with the live test ignored.
Protected qualification fixtures and production planner enforcement are unchanged.

No integration, main merge, publication or live-provider acceptance event is recorded.

## Final Local Validation

Independent exact-candidate spec-compliance and quality review approved the
complete candidate with every finding resolved. `task verify` passes, including
1,347 library tests, 106 CLI tests, 71 offline qualification tests (live ignored),
and the remaining integration/governance targets. `task docs:check`,
`task check:all-targets` and `git diff --check` pass. All 24 reviewed file hashes
remained unchanged throughout final verification.

This completion record changes only the spec, status-catalog rationale and author
work record. Runtime, test sources, Cargo/version inputs and Task commands remain
identical to the fully verified candidate. Reuse that proven-fresh runtime
evidence and renew exact review plus affected documentation/workspace gates for
these records. Memory Impact remains updated for the durable catalog and private
continuation contracts. No paid qualification, shared integration, main merge
or publication is established.
