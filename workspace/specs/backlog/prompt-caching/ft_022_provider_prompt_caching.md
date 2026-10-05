# FT-022: Provider Prompt Caching

**Status:** Backlog — specification requested on 2026-10-02. This is a proposal;

State: backlog
Primary Feature: prompt-caching
implementation and paid qualification have not started. Provider coverage,
retention policy, and evaluation targets must be settled before development.

## Summary

Make nib's model requests reuse stable prompt prefixes, enable supported provider
cache controls, and show measured cache reads and writes. Preserve current
instructions, authoritative session state, and complete-request context limits.
Caching should reduce repeated input processing during planning and execution
without changing what work is authorized or what counts as verified completion.

## Problem and Current Evidence

Repeated model turns resend guidelines, project references, tool definitions,
skills, memory, and conversation history. Provider prompt caching can reuse the
processing of an unchanged prefix. It is not a pool of tokens the agent can spend,
a saved answer, or a replacement for sending required context.

The inspected working tree already has partial support:

- `src/llm/types.rs` defines validated `LlmUsage.cached_input_tokens`, including
  checked aggregation that preserves missing breakdowns as unknown.
- `src/llm/openai.rs` and `src/llm/responses.rs` read cached-token details;
  `src/llm/gemini.rs` reads `cachedContentTokenCount`.
- `src/llm/anthropic.rs` reads cache creation and cache read counts. Creation is
  included in effective total input but is not retained as a separate usage field.
- `src/context/snapshot.rs` persists `context_usage` events and presents request
  and cumulative cached input in context inspection. This feature must extend
  that path rather than introduce a second usage ledger.
- `RuntimeContextSections::render` in `src/context/mod.rs` places Current Task
  directly after agent guidelines, before project docs, skills, and memory.
  Changing the task can disrupt reuse of those later sections.
- No `prompt_cache_key`, `prompt_cache_retention`, `prompt_cache_options`, or
  `cache_control` request controls were found in `src/`. There is no explicit
  provider cache policy or cache-resource lifecycle.

Consequently, nib may already receive implicit provider cache hits; code inspection
does not establish the actual hit rate or savings of a user's session. Direct
Anthropic requests currently lack the opt-in cache control documented by that
provider. No live calls were made for this analysis.

Related contracts: [T022 provider-neutral usage](../../done/llm-providers/T022_provider_neutral_llm_contract_and_adapter_conformance.md),
[T042 context budgeting and visibility](../../done/context-memory/T042_context_budgeting_and_live_visibility.md),
[T023 live qualification](../../development/llm-providers/T023_live_llm_provider_model_integration_qualification.md),
and [FT-021 model escalation and budgets](../model-routing/ft_021_cost_controlled_model_escalation.md).

## Users, Goals, and Non-Goals

Primary users are developers and other nib users running repeated agent turns.
Consumers include planning, execution, compression, delegated runs, and budget
accounting. nib remains an AI agent; coding is the initial evaluation workload.

Goals:

- Preserve useful stable prefixes across adjacent requests where inputs agree.
- Enable cache controls only for reviewed provider/transport/model combinations.
- Expose reported cache reads, writes, and unknown usage through shared interfaces.
- Measure total cost and latency alongside verified task outcomes.
- Keep changed instructions and repository evidence fresh.

Non-goals:

- Persisting or managing local model KV tensors, downloading weights, or caching
  model-generated answers instead of running verification.
- Gemini explicit cache resources and their creation/storage/deletion lifecycle.
- Cross-project/profile cache sharing, automatic model changes, or FT-021 routing.
- Extra warm-up requests, irrelevant prompt padding, or periodic paid cache refresh.
- A guaranteed hit rate, universal retention setting, or fixed savings percentage.

## Proposed Design

### 1. Stable Prefix and Live Context

Within each prompt family, separate reusable instructions/reference material from
changing task state. Render nib's fixed instructions, applicable project agent
guidelines, relevant project references, active skills, and approved profile memory
before current goal/step, workload status, changing attachments, and observations.
Keep prior conversation messages unchanged and append new messages when the
budget and provider continuation contract permit it.

This order is a projection contract, not permission to promote untrusted file or
tool content into instructions. Preserve current role boundaries, instruction
precedence, clarification state, and provider-specific continuation envelopes.
Providers may render tools before messages; stabilize the actual adapter payload,
not only the human-readable prompt.

Preserve semantic ordering for instruction scopes and skills. Canonicalize only
unordered collections and schema object keys; use deterministic tool selection
and ordering without advertising inactive tools or modifying validation meaning.
The same selected inputs and budget must produce the same reusable wire prefix.
Keep volatile timestamps, request IDs, and progress text out of that prefix.

Reload/revalidate inputs under existing scope rules. Changes to guidelines,
references, memory, skills, tool definitions, or relevant configuration must alter
the corresponding prefix immediately. Never pin stale content to retain a hit.
Fresh subagent context remains independent; cache reuse cannot transfer parent
permissions, plans, private history, or verification authority.

T042's full-request admission, response reserve, and compaction rules remain
authoritative. Cached input still occupies the context window. Do not subtract
cache hits from occupancy, delay required compaction, or retain irrelevant history.
Budget changes and compaction may intentionally reduce reuse. Record such local
changes as facts, not as proof of a provider cache miss reason.

### 2. Policy and Provider Adapters

Propose optional validated per-provider cache settings with `off` and `auto`
policies. Omission defaults to `off` for new explicit controls during initial
rollout. Stable ordering and existing usage parsing apply independently.
`off` means nib sends no optional cache controls; it cannot disable a provider's
implicit caching or promise zero provider retention.

`auto` chooses a reviewed mapping for the exact provider, endpoint trust scope,
transport, and model configuration. Separate structural adapter support from live
model qualification. Unknown compatible endpoints receive no speculative fields.
Explicit unsupported retention requests fail configuration validation before I/O;
automatic policy with no qualified mapping remains ordinary dispatch and reports
controls unavailable. Do not retry a rejected request with silently changed policy.

Initial proposed provider scope:

| Provider path | Proposed behavior |
| --- | --- |
| Direct OpenAI Responses and Chat Completions | Preserve implicit caching; add only reviewed optional key, retention, and breakpoint controls for the selected API/model. |
| Direct Anthropic Messages | Enable documented `cache_control` in `auto`; start with short retention and qualified growing-history behavior, adding a stable-block breakpoint when evidence warrants it. |
| Direct Gemini GenerateContent | Preserve implicit caching and report supplied usage; create no remote cache resources. |
| Grok, Meta, OpenRouter, custom compatible endpoints | Stable payloads and existing reported usage; explicit controls require their own official evidence and conformance fixtures. |
| Mock | Deterministic policy/usage fixtures; no claim of real provider caching. |

Use a stable opaque key where supported and appropriate, scoped to provider/account
configuration, endpoint, project, and profile. Do not include paths, prompts,
credentials, or human identifiers. Persist any key namespace seed only within
existing protected local state and never persist credentials. Do not change keys
on every turn; key rotation occurs when the isolation scope changes. Provider keys
are not an authorization boundary or proof of cache residency.

Longer retention is an explicit user setting subject to provider/model support
and the existing external-data policy. Preserve Responses `store: false`; that
field is distinct from prompt-cache retention. Automatic policy cannot enable a
new external destination or override a configured data-sharing restriction.

### 3. Usage and User Visibility

Retain normalized `input_tokens`, `output_tokens`, and `total_tokens`. Add an
optional cache-write input category to the typed usage contract and compatible
session evidence. When a provider reports retention-specific write counts, retain
a bounded breakdown sufficient for pricing rather than discarding it.

Normalize to disjoint uncached input, cache-read input, and cache-write input only
when the wire contract establishes those categories. Their sum must equal effective
input; reject impossible negative, oversized, overlapping, or inconsistent counts.
Read/write counts are subsets of input, not extra tokens to add again. Omitted
fields remain unknown unless a reviewed API contract defines omission as zero.

For Anthropic, keep its existing effective-input calculation and retain creation
tokens separately. Extend OpenAI write reporting when the selected API reports it.
Keep complete and streaming paths equivalent, and reconcile validated terminal
usage once per request. Duplicate events, retries, cancellation, and restart must
not fabricate zero billing or double-count a known request. Failed attempts with
unavailable usage retain unknown consumption under the existing ledger contract.

Expose per-request and accumulated cache reads/writes through existing `/context`
inspection and shared plain/TUI data. Identify unknown/partial totals and bounded
history coverage. Show a read ratio only for the same known denominator:
`reported cache-read input / reported effective input`; zero input yields no ratio.
Do not call a configured policy or predicted prefix match a cache hit.

FT-021 may consume these categories for cost/budget accounting. Any estimate must
use dated, reviewed prices for uncached input, cache reads, cache writes/retention,
and output, plus other applicable charges. Report unknown cost if required prices
or usage are absent. Include writes and failed attempts when comparing aggregate
spend; cache reads alone are not evidence of net savings or a billing receipt.

## Affected Areas

- `src/context/mod.rs`, `src/context/budget.rs`: stable/dynamic projection,
  deterministic selection, freshness, and admission.
- `src/llm/types.rs`, `registry.rs`, `factory.rs`, and provider adapters:
  typed policy/capability mapping, wire controls, complete/stream usage.
- `src/config/` and doctor diagnostics: optional settings, validation, support
  visibility, endpoint scope, and backward compatibility.
- `src/context/snapshot.rs`, `src/session/`, planner/loop/compression callers:
  additive usage evidence and request identity reconciliation.
- Shared interactive/context inspection and thin plain/TUI renderers: readable
  cache evidence without a separate billing ledger.
- Existing provider, context, session, interaction, and documentation fixtures.
  Update user/config and architecture references when implementation ships.

No changes to governed agent instructions, tool approval rules, workload states,
or external integration authority are proposed.

## Alternatives and Risks

Implicit caching alone needs fewer controls and already provides possible benefit,
but leaves Anthropic activation and write accounting unresolved. Cache controls
alone cannot repair a changing prefix. Local request-body caching saves assembly
work but does not establish provider input savings. Remote explicit cache objects
add storage costs and cleanup/recovery obligations and are deferred.

Primary risks are stale context, altered instruction precedence, unsupported gateway
fields, cache-write costs outweighing reads, and retention beyond user expectations.
Mitigate with fresh input validation, role/precedence regression fixtures, exact
capability mappings, opt-in controls, reviewed retention, and representative
measurements. Keep cache metadata bounded/redacted; do not persist raw prompt
hashes or provider continuation objects as new audit fields.

## Compatibility, Rollout, and Version Impact

1. Establish deterministic request fixtures and baseline measurements; extend
   usage without enabling new controls.
2. Refactor stable-prefix layout while preserving prompt semantics and budgets.
3. Add opt-in reviewed direct-provider controls and diagnostics.
4. Qualify representative coding sessions, document results and supported scope,
   then separately decide whether explicit controls should become a default.

Existing configurations and sessions remain readable. Additive optional usage
fields require review of strict deserializers and any public serialized contract;
older clients must receive a supported projection or documented schema transition.
No historical usage may be rewritten to invent cache writes. Disabling controls
restores ordinary dispatch while preserving recorded usage; it does not purge
provider caches. Provider retention/deletion guarantees remain provider contracts.

| Component | Proposed impact | Rationale |
| --- | --- | --- |
| nib CLI/library | Minor on implementation, subject to current release policy | New optional cache controls and additive usage visibility; public schema compatibility must be resolved. |
| This backlog proposal | None | Documentation only; no package bump or version reservation. |

## Acceptance Criteria

- [ ] Identical reusable inputs and admitted section allocations yield an identical
  wire prefix; changing only the task/step preserves it when those allocations
  still fit. Required budget-driven changes remain explicit and safe.
- [ ] Changing instructions, references, skills, tools, memory, profile, or endpoint
  refreshes/invalidate the relevant projection or key scope without stale reuse.
- [ ] Applicable instruction order, roles, clarification, tool schemas, plan binding,
  and fresh delegation context remain correct under the new projection.
- [ ] Omitted/off settings send no optional fields; auto uses qualified mappings;
  unsupported explicit settings fail locally and gateways receive no guessed fields.
- [ ] Complete and streaming fixtures preserve distinct read/write categories,
  unknowns, normalized input totals, and exactly-once accounting.
- [ ] Context occupancy includes cached input; compaction and undersized-budget
  rejection remain enforced, including provider continuation requests.
- [ ] Existing configs/sessions load; restart, retries, and cancellation preserve
  truthful partial usage and do not silently replay warm-up calls.
- [ ] Plain/TUI context inspection agrees with persisted evidence, including missing
  counts, zero-input ratios, partial totals, and bounded history coverage.
- [ ] A bounded, authorized exact-model comparison reports reads, writes, latency,
  total priced cost where known, and verified task outcomes. It meets predeclared
  targets before savings or default-enable claims are made.

## Validation Gates

For this spec change, run `task docs:check` and `task verify`. Review scope and
acceptance testability before handing off the proposal.

Implementation requires credential-free request-body and complete/stream fixtures
for every newly supported mapping, plus context freshness/order/budget, session
compatibility, and interaction tests. Use `task test:llm-conformance`,
`task test:agent-context`, relevant session/interactive tasks, `task docs:check`,
and final `task verify`. Add a focused Task entry if no existing entrypoint covers
new caching tests. Spec-compliance review precedes code-quality review.

Live evaluation follows T023's existing credential, approval, privacy, request,
output, and spending requirements. Measure cold/warm adjacent turns, changed task,
changed tool/instruction inputs, compaction, expired cache, and model/endpoint
changes. A missing hit is not by itself a transport failure; retain actual results.
Offline fixtures prove request/accounting behavior, not live cache availability.
This proposal authorizes no paid requests.

## Open Decisions Before Development

1. Confirm initial provider/model/transport coverage and reviewed capability metadata.
2. Confirm opt-in rollout, short retention defaults, and allowed longer retention
   under the project's external-data policy.
3. Select a representative task set, verified-quality tolerance, savings target,
   latency target, and approved evaluation budget before live measurements.
4. Resolve the public usage schema transition and retention-specific write breakdown
   with FT-021 so accounting has one authoritative contract.

After these decisions and a scoped implementation plan, the next allowed lifecycle
transition is `backlog -> development`. No implementation starts from this proposal.

## Prior Memory Notes

Pending. The request and current evidence are captured here. Do not record proposed
defaults or provider coverage as adopted decisions. Reconcile approved stable policy
into `workspace/agents/memory/` during development without storing credentials or request text.

## Provider References

Official guidance reviewed on 2026-10-02; mappings must be checked again when
implementation begins. Model minimums, retention, and prices are provider-specific.

- [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching):
  prefix reuse is implicit. Earlier models use stable keys for routing; GPT-5.6
  and later route automatically and keys can separate accounting. Earlier retention
  uses `prompt_cache_retention`; newer controls use `prompt_cache_options` and
  support explicit breakpoints. Newer models report cache writes as well as reads.
- [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching):
  activation uses top-level automatic `cache_control` or content-block breakpoints.
  The prefix covers tools, system, then messages; reads and creation are separately
  reported. Short and longer retention have different write charges.
- [Gemini GenerateContent caching](https://ai.google.dev/gemini-api/docs/generate-content/caching):
  supported models offer implicit caching; explicit caching creates separate remote
  resources. Cache tokens still count toward ordinary token limits.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | none | none | Backlog proposal only; no shipped artifact changes until separately scoped development and reservation. |

## Memory Impact

Status: none
Rationale: The proposal records open choices without adopting new policy; approved durable decisions will be classified during development.
