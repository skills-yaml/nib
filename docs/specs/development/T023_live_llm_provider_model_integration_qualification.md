# T023: Live LLM Provider and Model Integration Qualification

**Status:** Development

**Related:**
[FT-004: LLM Integration and Agent Loop](../done/ft_004_llm_integration_and_agent_loop.md),
[FT-011: LLM Streaming and TUI](../done/ft_011_llm_streaming_and_tui.md),
[T007: Configuration Schema Alignment](../development/T007_configuration_schema_alignment_and_nib_doctor_validation.md),
[T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](../development/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md),
[T022: Provider-Neutral LLM Contract and Adapter Conformance](../development/T022_provider_neutral_llm_contract_and_adapter_conformance.md),
[Task Runner](../../tech/task.md), and
[CI](../../tech/ci.md)

## Summary

Add a credential-gated live integration qualification system that discovers the model
catalog visible to the test account for OpenAI, Anthropic, Google Gemini, xAI/Grok, and
Meta, then exercises every relevant model through nib's production LLM factory and
adapter. OpenRouter is intentionally narrower: its live matrix runs only a reviewed,
exact-ID allowlist while still checking that every allowlisted model exists in the live
OpenRouter catalog.

Each discovered model receives an explicit outcome. Text-generation models compatible
with a nib transport must pass completion, streaming, and applicable tool-continuation
scenarios. Non-generative or transport-incompatible catalog entries are recorded as
not applicable or unsupported with evidence; authentication, billing, rate-limit,
region, catalog ambiguity, and budget failures are blockers, never silent skips.

The live suite is scheduled and manually runnable, but never runs as part of ordinary
PR, `task check`, or `task test` validation. Deterministic credential-free fixtures in
T022 remain the release-authoritative protocol and safety evidence; live results add
time-bounded compatibility evidence for mutable provider/model combinations.

This spec interprets the requested OpenRouter "redact list" as a **reduced,
restricted, reviewed allowlist**, not as log redaction. If a different meaning was
intended, the allowlist policy is the only design section that needs revision.

## Problem Statement

nib can prove adapter behavior against local fixtures, but fixtures cannot prove that
the current hosted APIs accept nib's requests for every model currently visible to an
account. Provider catalogs, aliases, transport support, tool support, and regional
availability change independently of nib releases. The current static registry also
cannot establish that a model still exists or works with its configured adapter.

Without a live qualification layer:

- a provider can add a model that nib never exercises;
- a previously working model or alias can be removed or change compatibility;
- completion can work while streaming or tool continuation fails;
- the LLM factory can resolve a model to the wrong endpoint or transport;
- a provider catalog can be only partially read because pagination was missed;
- a missing secret, exhausted account, rate limit, or regional restriction can be
  mistaken for model incompatibility;
- OpenRouter's very large and fast-changing catalog can create unbounded cost and an
  unreviewable support claim; and
- ad hoc local commands can leak credentials, model responses, remote error bodies, or
  private fine-tuned model identifiers into logs and CI artifacts.

The requirement is not satisfied by a single provider canary or by testing only the
hard-coded model suggestions. It needs a repeatable inventory, an exhaustive accounting
of that inventory, controlled live calls through production code, and a safe report.

## Decision

Create a separate live LLM qualification harness with four explicit modes:

1. **Catalog:** discover, paginate, normalize, deduplicate, and classify models from
   catalog metadata without making generation requests. Entries whose catalogs expose
   insufficient capability metadata remain `requires_probe`, not guessed.
2. **Canary:** run the qualification scenarios against each provider's configured
   default model and every OpenRouter allowlist entry. This is a fast credential and
   endpoint health signal.
3. **Selected:** run the reviewed core nib task suite against every exact model in the
   external selected matrix for all six providers. This is the bounded CI regression
   signal and never substitutes another model for a missing selection.
4. **Full:** run the applicable scenarios against every text-generation model in the
   completed account-visible catalog for OpenAI, Anthropic, Gemini, Grok, and Meta, and
   against every OpenRouter allowlist entry.

The harness owns provider catalog decoding and orchestration only. Every model request
must be built with the typed T022 request contract and sent through the same provider
registry, configuration resolution, factory, HTTP/SSE codecs, retry policy, terminal
validation, redaction, and tool-continuation path used by nib at runtime. A parallel
"test client" that bypasses production adapters is prohibited.

Live qualification is compatibility monitoring, not the sole correctness gate. A live
pass expires as evidence when its provider/model/catalog snapshot or source revision
changes. A live failure does not justify weakening deterministic validation, changing
request semantics, or automatically falling back to a different model or provider.

## Definitions and Scope Boundary

- **Account-visible catalog:** the complete, fully paginated list returned to the exact
  credential used by one run. It is not a claim about models available to every account,
  region, pricing tier, or date.
- **Catalog entry:** one unique provider-returned canonical model ID. Aliases are stored
  as metadata and are not executed as duplicate models when the provider identifies a
  canonical ID.
- **Transport profile:** a provider, model, and nib wire transport combination, such as
  OpenAI Responses, OpenAI Chat Completions, Anthropic Messages, Gemini
  GenerateContent, or an OpenAI-compatible Chat endpoint.
- **Eligible profile:** a catalog entry for which authoritative catalog metadata or a
  bounded live probe establishes that the nib transport can perform text generation.
- **Advertised model:** a default or suggested model present in nib's provider registry,
  or an exact OpenRouter allowlist entry. Advertised models have stricter pass criteria
  than newly discovered catalog entries.
- **Complete catalog run:** every catalog page was read and every unique entry received
  an evidence-backed eligibility classification. `requires_probe` is allowed because
  catalog mode makes no generation requests.
- **Complete full run:** every catalog page was read and every unique entry received a
  terminal qualification classification; every eligible profile ran every required
  scenario; no result ended `requires_probe`, blocked, unknown, or budget-truncated.

"All models" means all entries visible in the catalog snapshot, not a source-controlled
list. Every entry must be accounted for. Image-only, audio-only, embedding, moderation,
reranking, and other non-text-generation entries are not sent a chat request, but they
must appear in the report as `not_applicable` with the catalog capability that justified
the classification. Private or fine-tuned entries visible to the dedicated test account
are included; their identifiers are pseudonymized in persisted artifacts.

A model added after catalog capture is naturally covered by the next run. A model that
disappears between discovery and execution produces `catalog_drift` and makes the run
incomplete; it is not silently removed from the denominator.

## Goals

- Automatically discover the current account-visible model catalog for every supported
  direct provider and completely account for it.
- Exercise every eligible provider/model/transport profile through nib's production
  LLM layer.
- Prove basic completion, streaming, normalized terminal handling, structured tool
  calls, and native tool-result continuation wherever applicable.
- Detect drift in provider defaults, registry suggestions, aliases, endpoint behavior,
  and OpenRouter allowlist entries.
- Keep OpenRouter execution bounded to a reviewed list of exact canonical model IDs.
- Produce a deterministic machine-readable result and a concise CI summary without
  secrets, raw remote bodies, private continuation state, or unbounded model output.
- Distinguish product incompatibility from infrastructure, credential, billing, quota,
  region, and budget blockers.
- Bound requests, output tokens, attempts, concurrency, duration, and projected cost.
- Keep ordinary local and PR validation free of network calls, secrets, provider
  availability, model nondeterminism, and paid usage.

## Non-Goals

- Replacing T022's credential-free conformance suite, fixtures, or safety gates.
- Benchmarking model intelligence, quality, latency rankings, or cost efficiency.
- Asserting byte-for-byte equality between responses from different requests or models.
- Testing embeddings, image generation, audio, video, reranking, fine-tuning operations,
  batch, file, provider-hosted tools, web search, or computer use.
- Automatically adding newly discovered models to nib's user-facing registry or to the
  OpenRouter allowlist.
- Maintaining a source-controlled catalog for direct providers.
- Testing every OpenRouter model, wildcard family, alias, free variant, or routed
  fallback.
- Sending production data, repository content, user prompts, real tools, or mutating
  tool calls to a provider.
- Proving provider-side privacy, retention, billing accuracy, or service-level
  objectives.
- Running paid live calls on untrusted pull requests or forks.
- Automatically changing runtime defaults, disabling tools/reasoning, or switching
  transport/provider when a test fails.

## Proposed Design

### Harness Boundary

Add a live-only integration target and support modules under `tests/`. The live test is
marked ignored and additionally requires an explicit enable flag so an accidental
`cargo test -- --ignored` does not incur cost. It may reuse the production public API
or a narrowly feature-gated test-support surface from `src/llm`, but qualification-only
code must not be compiled into the release binary.

The orchestrator performs this fixed flow:

1. Load live-test settings and environment credentials without reading or writing the
   project `.nib/config.toml`.
2. Resolve the production provider descriptor, endpoint, transport defaults, and safe
   diagnostic context.
3. Fetch every catalog page under response byte, item, page, and deadline limits.
4. Normalize entries, preserve provider order only as metadata, deduplicate canonical
   IDs, and reject duplicate IDs with conflicting metadata.
5. Determine candidate transport profiles from structural adapter capabilities and
   catalog metadata. Where the catalog lacks sufficient metadata, perform a minimal
   bounded text-generation probe rather than use model-name prefixes.
6. Execute required scenarios for each eligible profile with provider-local bounded
   concurrency and the production retry policy.
7. Reconcile every catalog entry to one terminal classification, enforce completeness
   and advertised-model rules, scan the report for sensitive values, and write results
   atomically.

The harness must never infer support from strings such as `gpt`, `claude`, `gemini`,
`grok`, `llama`, version suffixes, or ownership labels alone.

### Provider Catalog Discovery

#### OpenAI

- Fetch `GET /v1/models` using the live OpenAI credential.
- Treat the returned IDs as the complete account-visible inventory for that response.
- The documented model object has only basic identity/ownership metadata, so it is not
  sufficient to classify text, tool, or transport support.
- Attempt a bounded basic probe for each structurally supported OpenAI transport. Only
  a documented machine-readable endpoint/model incompatibility may produce
  `unsupported_transport`; an ambiguous `4xx` is a failure.
- Run the remaining scenarios for every transport profile whose basic probe succeeds.

#### Anthropic

- Fetch every page from `GET /v1/models` using `after_id` until `has_more` is false.
- When `has_more` is true, use the returned `last_id` as the next `after_id`; do not
  derive a cursor from model order or ID syntax.
- Reject non-progressing cursors, repeated pages, contradictory duplicate entries, and
  a missing next cursor when `has_more` is true.
- Use returned capability metadata when available, but still execute the production
  Anthropic Messages path for every eligible model.

#### Google Gemini

- Fetch every page from `GET /v1beta/models` using `nextPageToken`.
- Select entries whose returned supported generation methods/actions include
  `generateContent`; record all other entries as `not_applicable`.
- Preserve the returned `name` as the canonical catalog identity and the returned
  `baseModelId` as the documented generation target. The request model ID uses
  `baseModelId` when present; a missing or contradictory mapping is malformed catalog
  evidence rather than permission to infer an ID from a display name.
- Distinct catalog entries that map to one `baseModelId` remain distinct accounting
  records. They may share one paid scenario result only when provider metadata proves
  they are aliases for the same generation target; otherwise each entry is executed.
- Reject repeated page tokens, contradictory duplicate entries, and malformed
  capability metadata.

#### xAI/Grok

- Fetch `GET /v1/language-models`, which is narrower and more informative than the
  generic models endpoint for this requirement.
- Use the returned canonical model ID for execution and retain aliases only as catalog
  metadata. Do not run an alias separately when it resolves to the same canonical ID.
- Probe every nib transport that xAI structurally supports; a successful profile must
  run all remaining applicable scenarios.

#### Meta

- Resolve the Meta Model API root through the same endpoint-security rules as the
  production Meta adapter, then fetch its authenticated OpenAI-compatible `/models`
  resource.
- Until an authoritative Meta catalog endpoint and response fixture are verified, the
  workflow requires `NIB_LIVE_META_BASE_URL` and treats missing or incompatible catalog
  support as `blocked_configuration`; it must not substitute OpenRouter's Meta catalog,
  a Llama download list, or a hard-coded Muse/Llama list.
- Once Meta publishes a verified stable default root and catalog contract, record the
  dated evidence in this spec before making the explicit test endpoint optional.
- Exercise returned text-generation candidates through nib's distinct Meta provider
  adapter. OpenAI wire compatibility does not allow relabeling the provider as OpenAI.

#### OpenRouter

- Fetch `GET /api/v1/models?output_modalities=all` so catalog reconciliation does not
  depend on OpenRouter's default text-output filter. Normalize the returned canonical
  slugs, supported parameters, modalities, pricing, and expiration metadata.
- Do not execute every discovered OpenRouter entry. Join the catalog against the
  checked-in reviewed allowlist and run only exact matches.
- A missing, expired, duplicate, non-canonical, modality-incompatible, or capability-
  incompatible allowlist entry is a hard failure. New non-allowlisted catalog entries
  are reported as informational catalog drift and never run automatically.
- OpenRouter routing metadata and the identity of an underlying routed provider remain
  private transport state and are not acceptance evidence for a direct provider.

### OpenRouter Reviewed Allowlist

Create `tests/fixtures/llm_live/openrouter_models.toml` with strict schema validation.
Each entry contains:

- exact canonical model ID (no glob, family prefix, `latest`, `auto`, or fallback list);
- allowed nib transport(s);
- required scenarios (`complete`, `stream`, `tool`, and `tool_continuation`);
- required catalog parameters/modalities;
- rationale for inclusion;
- review owner;
- review date and expiry date; and
- optional per-model projected-cost ceiling when catalog pricing is available.

The initial allowlist is produced by validating the OpenRouter models currently
advertised by nib against the live catalog. Invalid or retired entries must be replaced
through review; they must not be silently normalized to a similarly named model.

Allowlist changes require an ordinary reviewed repository change. CI validates syntax,
unique IDs, dates, bounded strings, supported transports, exact catalog membership,
and expiry. A scheduled job may publish suggested additions or removals in its artifact,
but it does not edit the file or open a support claim automatically.

### Qualification Scenarios

Every scenario uses synthetic, short, non-sensitive input and a fresh cryptographic
nonce. Prompts, tool definitions, and maximum output sizes are constant except for the
nonce and provider-neutral transport requirements.

#### 1. Complete Text

- Ask for a short response containing the exact ASCII nonce.
- Set a small maximum output token limit through the typed contract.
- Use explicit temperature zero only when the model/transport reports or proves it can
  represent that value; otherwise request the provider default rather than dropping a
  configured option.
- Require a validated completed private turn, a safe recognized terminal outcome,
  bounded non-empty text containing the nonce, no tool calls, and sane non-negative
  usage values when usage is returned.

This scenario is also the capability probe when the catalog cannot identify text
generation support. A benign safety refusal is recorded as a model failure because the
fixed probe contains no risky content; the harness does not retry with a different
prompt.

#### 2. Streamed Text

- Send a new request with a different nonce through the production stream path.
- Require bounded UTF-8-safe public deltas, exactly one private terminal result, prompt
  nonce presence in the reconstructed sanitized text, and no event after termination.
- Assert structural parity with `complete` (valid completion and finish class), not
  byte-for-byte content equality between two independent generations.
- Fail premature EOF, duplicate terminal delivery, malformed chunks, remote in-band
  errors, and terminal states that would authorize an incomplete turn.

#### 3. Single Tool Call and Continuation

- Offer one inert `record_probe` function with a strict object schema requiring the
  nonce and no additional properties.
- Force that tool only when the provider/model contract supports tool choice; otherwise
  use a fixed prompt that unambiguously requests it.
- Require exactly one validated provider-neutral tool call containing the nonce, a nib
  `ToolInvocationId` distinct from private provider correlation, and no execution by
  `ToolExecutor`.
- Return a synthetic successful `ToolResult` containing a second receipt nonce through
  the provider's native continuation path.
- Require a final validated answer containing the receipt nonce and no second tool call.

If authoritative catalog metadata says tools are unsupported, classify the scenario as
`not_applicable` while retaining a passing text profile. If an advertised nib model or
OpenRouter allowlist entry is intended for agent use, tool and continuation support are
mandatory; `not_applicable` is then a hard support failure.

#### 4. Parallel Tool Correlation

For models whose catalog metadata and nib transport both claim parallel function tools,
request two inert tools with distinct nonces, return results in the provider-required
order, and require exact one-to-one correlation and a final receipt. Models without a
positive parallel-tools claim record `not_applicable`. T022 fixtures remain responsible
for malformed, duplicate, missing, foreign, and replayed correlation cases; the live
test does not intentionally send invalid paid requests.

### Selected Nib Task Suite and Model Matrix

The bounded CI regression suite is repository-owned external configuration at
`tests/fixtures/llm_live/selected_models.toml`. It is data rather than Rust source so a
maintainer can update model selection through ordinary review without changing harness
logic. The file has a strict versioned schema, suite ID, owner, review/expiry dates,
required and conditional scenario sets, and an exact non-empty model list for every
network provider. Wildcards, aliases such as `latest`, duplicate IDs, unknown providers,
unknown scenarios, expired reviews, and an omitted provider fail before network I/O.

The initial `nib-llm-core-v1` suite defines the tasks nib's LLM layer must complete:

1. `complete_text`: return a bounded non-streamed answer containing the run nonce and
   an authoritative successful terminal status.
2. `streamed_text`: emit ordered text deltas containing a distinct nonce and exactly one
   authoritative terminal outcome.
3. `single_tool_continuation`: emit one schema-valid inert tool call, preserve the exact
   neutral call/result correlation through the provider-native continuation path, and
   return the final receipt nonce. This is required for every selected model because it
   is the minimum agent execution profile.
4. `parallel_tool_continuation`: preserve two distinct tool call/result correlations and
   finish with the receipt nonce. This is conditional and runs only when authoritative
   catalog metadata positively advertises parallel tool calls; a claimed capability
   that fails the task fails the profile.

`selected` mode executes the three required tasks on every configured provider/model
and production transport. It adds the conditional task where advertised. Exact selected
IDs must be present in the same run's live catalog; there is no fallback to a provider
default, alias, family, or replacement. Selected OpenRouter IDs must also be present and
approved in the reviewed OpenRouter allowlist, so the matrix cannot bypass its separate
cost/capability policy.

The initial selected model set tracks the bundled default for each direct provider and
the four proposed OpenRouter family representatives, which remain blocked by the
separate approval gate:

- OpenAI: `gpt-5.6-sol`
- Anthropic: `claude-opus-5`
- Google: `gemini-3.6-flash`
- Grok/xAI: `grok-4.5`
- Meta: `muse-spark-1.1`
- OpenRouter: `openai/gpt-5.6-sol`, `anthropic/claude-opus-5`,
  `google/gemini-3.6-flash`, and `x-ai/grok-4.5`

Each selected JSON/Markdown report records the suite ID, SHA-256 matrix fingerprint,
review/expiry dates, and task counts. Provider artifacts from an aggregate run must have
the same fingerprint. This makes historical evidence attributable to the exact selected
list even after the external configuration changes.

### Classification and Pass Rules

Each catalog entry and transport profile ends in exactly one bounded local enum:

- `qualified`: every required applicable scenario passed;
- `requires_probe`: catalog metadata cannot establish text-generation or transport
  eligibility; this is terminal only for catalog mode and must be resolved in canary or
  full mode before that run can pass;
- `not_applicable`: catalog metadata proves the entry is outside text generation or a
  specific optional capability;
- `unsupported_transport`: a documented machine-readable response proves the model
  cannot use that nib transport;
- `failed_adapter`: the provider accepted the profile but a completion, stream,
  terminal, tool, continuation, limit, or parsing invariant failed;
- `catalog_drift`: the entry changed or disappeared after catalog capture;
- `blocked_auth`, `blocked_quota`, `blocked_billing`, `blocked_region`,
  `blocked_rate_limit`, `blocked_configuration`, or `blocked_budget`;
- `unknown`: no safe deterministic classification was possible.

Remote message text is never used as the only classifier. Provider adapters may map
documented status, error code, header, and response shape to a local enum without
rendering the remote text.

A direct-provider full run passes only when:

- the catalog is complete and every entry has a terminal result;
- every eligible profile is `qualified`;
- all advertised models are present (or a documented canonical alias), qualify on their
  configured default transport, and pass the tool profile required by nib's agent loop;
- all non-eligible entries have evidence-backed `not_applicable` or
  `unsupported_transport` classifications; and
- no entry is `requires_probe`, blocked, drifted, failed, unknown, or omitted due to
  limits.

The OpenRouter full run applies the same rules to allowlist entries, not to the whole
catalog. The aggregate run passes only if all six provider jobs pass. Missing provider
credentials or a regionally unavailable Meta account makes the aggregate run blocked,
not green.

### Configuration and Secrets

The live suite consumes only process environment or CI environment-scoped secrets:

- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `GOOGLE_API_KEY`
- `XAI_API_KEY`
- `META_API_KEY`
- `OPENROUTER_API_KEY`
- `NIB_LIVE_META_BASE_URL` while Meta has no verified repository default

Keys must belong to dedicated low-privilege test projects/accounts with provider-side
spend and rate limits. They must not be user or production keys. The harness never
loads `.env`, never persists a key, never prints request headers, and never places
credentials in command-line arguments.

Live settings select provider, mode, concurrency, request/token/deadline ceilings,
results directory, and approved maximum projected spend. They cannot override the
catalog or generation endpoint with a URL containing credentials, a query, or a
fragment. Custom endpoint values are treated as sensitive diagnostic inputs and pass
through the production endpoint and redaction validation.

Protected CI live jobs additionally permit only built-in provider origins or an exact
reviewed HTTPS origin stored in environment configuration. Catalog clients reject
cross-origin redirects plus loopback, link-local, and private-network destinations;
local fixture tests use a separate credential-free path and cannot be selected by a
live workflow.

Outside CI, catalog mode requires the network acknowledgement. Canary and full modes
require both independent acknowledgements:

- `NIB_LIVE_TESTS=1` confirms that real network calls are intended; and
- `NIB_LIVE_ACK_COSTS=1` confirms that the caller accepts paid usage within configured
  ceilings.

The protected scheduled CI environment supplies equivalent non-interactive policy.
There is no interactive prompt inside CI, and a catalog-only job cannot select a
generation scenario even when the paid-cost acknowledgement is present globally.

### Cost, Rate, and Time Bounds

Before generation, the harness computes the exact number of discovered entries,
candidate profiles, minimum requests, maximum attempts, and maximum output tokens. It
uses catalog pricing where available to produce a conservative projected cost. Missing
pricing is labeled unpriced and requires an explicit provider-level unpriced allowance
backed by a provider-side spend cap.

Each run enforces:

- a fixed small output-token limit per request;
- at most one tool round and one parallel-tool round per applicable profile;
- production retry semantics with the existing global attempt bound;
- no retry for nonce mismatch, refusal, malformed output, or other semantic failure;
- provider-local concurrency (one by default) and bounded retry delays;
- per-request, per-model, per-provider, and whole-run deadlines; and
- provider request/token/projected-cost ceilings.

If preflight projections exceed a ceiling, the harness makes no generation calls for
that provider and returns `blocked_budget`. If a runtime ceiling is reached, remaining
entries are marked `blocked_budget` and the run is incomplete. A partial run can aid
diagnosis but can never be reported as a pass.

Where a provider does not expose reliable pricing, request and token ceilings plus the
provider account's hard billing limit are the authoritative safeguards. The report must
not claim exact spend from token estimates.

### Result Artifact and Redaction

Write a versioned JSON result plus a generated Markdown summary. The JSON contains:

- schema version, random run ID, source revision, platform, mode, and timestamps;
- provider and transport IDs;
- catalog page/item counts and a hash of normalized catalog metadata;
- one bounded record per model/profile/scenario;
- local classification, safe status/error class, attempts, duration, usage when
  returned, and projected cost when available; and
- completeness, allowlist, advertised-model, budget, and aggregate pass/fail results.

Do not persist raw request/response bodies, headers, remote messages, model reasoning,
stream chunks, prompts, provider call IDs, continuation state, tool arguments/results,
endpoint queries, or routed-provider metadata. Scenario nonces are discarded after the
assertion.

Public base-model IDs may be stored only after bounding, control escaping, and redacting
against every configured sensitive value. Entries identified as customer-owned,
fine-tuned, private, or otherwise non-public are represented in persisted artifacts by
a run-stable keyed digest and safe ownership class, not the raw ID. The in-memory map is
dropped at process exit.

Before publication, serialize into memory, scan the complete bytes for every raw and
encoded credential/sensitive value, apply a maximum artifact size, then publish with an
atomic no-follow write. Any scan hit fails the run and suppresses artifact upload.
The encoded-variant set is finite, versioned, and tested: trimmed/raw bytes, the
production percent-encoding/decoding variants, JSON-string escaping, and standard plus
URL-safe Base64 with and without padding.

### Task Runner and CI

Add canonical tasks with stable names:

- `task test:llm-live:offline`
- `task test:llm-live:catalog`
- `task test:llm-live:canary`
- `task test:llm-live:selected`
- `task test:llm-live:full`

The ordinary `task check`, `task test`, `task dev`, coverage, and PR workflows must not
invoke credentialed modes. Catalog/allowlist/matrix parsers and all orchestration logic
that can be tested without credentials remain part of ordinary deterministic tests and
are also exposed through the focused `offline` target.

Add `.github/workflows/llm-live.yml` with:

- `workflow_dispatch` inputs for provider and mode, including `selected`;
- a scheduled inventory run using protected credentials where the provider requires
  authentication, defaulting to catalog-only with generation structurally disabled;
- a reviewed post-rollout switch that may change the default-branch schedule from
  `catalog` to the bounded `selected` matrix, but never to unbounded `full`;
- one isolated job per provider with matrix fail-fast disabled;
- environment-scoped secrets, read-only repository permissions, no fork/PR trigger,
  bounded job timeouts, and one concurrency group per provider;
- a final aggregate job that fails on missing/blocked provider results and publishes a
  compact GitHub summary; and
- short-retention sanitized artifacts for trend and catalog-diff review.

A provider failure must not cancel other provider jobs. Workflow logs contain only
bounded progress counters, safe local classifications, pseudonymized private models,
and artifact locations. Debug HTTP logging is forcibly disabled even when a caller sets
generic Rust logging variables.

The first implementation may combine catalog and full schedules if every catalog
requires authentication; the semantic modes and reports remain separate.

### Relationship to T022

T022 owns the typed neutral contract, provider registry, distinct adapters, structural
capabilities, native tool continuation, terminal authority, safe errors, retries, and
credential-free conformance. T023 depends on those boundaries and must not duplicate or
weaken them.

T023 can land catalog parsers, the OpenRouter allowlist schema, dry-run planning, and CI
scaffolding while T022 is in development. Paid generation scenarios cannot become
release evidence until the corresponding T022 provider path passes deterministic
conformance. T022 can move to done without a T023 paid sweep because live paid calls
remain optional for deterministic releases; T023 cannot move to done until its own
exact-revision live acceptance matrix passes.

## Implementation Plan

1. Add failing deterministic tests for paginated catalog decoding, loop detection,
   duplicate/conflicting entries, normalization, classification completeness,
   OpenRouter allowlist validation, result bounds, pseudonymization, and secret scans.
2. Define live-only catalog, profile, scenario, classification, budget, and report
   types; implement a dry-run planner that performs no generation.
3. Implement catalog adapters for OpenAI, Anthropic, Gemini, xAI, OpenRouter, and the
   explicitly configured Meta root with bounded shared HTTP support and production
   endpoint/redaction rules.
4. Implement complete and streaming nonce scenarios through the production T022
   factory and contract.
5. Implement inert single/parallel tool and native continuation scenarios without
   invoking ToolExecutor.
6. Add completeness and advertised-model reconciliation, catalog drift comparison,
   cost/request/token/deadline guards, atomic sanitized artifacts, and deterministic
   local tests for every failure class.
7. Add the five Task targets and the protected manual/scheduled GitHub workflow.
8. Add the strict selected task/model matrix, selected planner mode, report provenance,
   and deterministic validation for every provider, task, and exact model selection.
9. Validate and review the initial OpenRouter exact-ID allowlist against the live
   catalog.
10. Run catalog, canary, selected, then full provider matrices; fix adapter defects
    rather than weakening assertions or excluding failing eligible models.
11. Perform independent spec-compliance and technical/security reviews, run canonical
    deterministic gates, record exact live evidence, and only then move T023 to done.

## Rollout Plan

### Phase 1: Offline Harness

Land schema, catalog fixtures, classification/reconciliation logic, report safety, dry
run, Task names, and ignored-test safeguards. Normal CI proves that paid calls cannot be
triggered accidentally.

### Phase 2: Catalog and Canary

Configure dedicated provider accounts and the protected CI environment. Run all catalog
jobs, resolve pagination/endpoint differences, validate the initial OpenRouter allowlist,
then enable default-model canaries. No full schedule is enabled while any catalog is
partial or any artifact redaction check is unresolved.

### Phase 3: Full Manual Qualification

Run one provider at a time with preflight budgets. Reconcile every result and fix nib
adapter defects. Provider outages and account blockers are recorded but do not cause
models to be removed from coverage.

### Phase 4: Scheduled Monitoring

Enable the weekly selected workflow only after one exact-revision aggregate pass and the
external credential/budget/approval gates. Keep full qualification manual. Review catalog
additions/removals, selected-matrix and budget changes, and OpenRouter allowlist expiry
through ordinary repository review. Live failures alert through the workflow result and
summary; automatic runtime fallback remains prohibited.

## Alternatives Considered

### Test Only Registry Models

Rejected. It would verify the current suggestions but miss new account-visible models
and silently preserve stale support assumptions.

### Hard-Code Every Provider Catalog

Rejected. Direct-provider catalogs and aliases change too often, differ by account and
region, and cannot support the requirement for automatic current coverage.

### Test Every OpenRouter Model

Rejected. OpenRouter exposes hundreds of heterogeneous models and routing variants.
Running all of them would create unbounded cost, rate pressure, noisy failures, and an
unreviewable product support claim. Exact reviewed IDs provide intentional coverage.

### Use OpenRouter to Test Other Providers

Rejected. A routed OpenRouter success exercises OpenRouter's adapter and routing, not
the direct OpenAI, Anthropic, Gemini, xAI, or Meta credentials, endpoints, errors, and
continuation behavior.

### Use Only Mocked Provider Fixtures

Rejected as the complete solution. Fixtures are essential deterministic evidence but
cannot detect current hosted catalog and model/transport compatibility drift.

### Run Live Tests on Every Pull Request

Rejected. Fork security, secret exposure, cost, rate limits, provider incidents, and
model nondeterminism make paid external calls unsuitable as a PR merge gate. Offline
harness logic and fixtures remain PR gates; protected scheduled/manual runs provide
hosted evidence.

### Treat Every 4xx as Unsupported

Rejected. Authentication, billing, region, quota, safety, malformed requests, and
adapter bugs frequently surface as 4xx responses. Only documented structured evidence
can classify a transport as unsupported.

## Risks and Tradeoffs

- **Cost growth:** Direct catalogs may expand suddenly. Mitigation: preflight the whole
  matrix, use tiny bounded scenarios, enforce ceilings, fail incomplete, and use hard
  provider account budgets.
- **Provider and model nondeterminism:** A valid model may occasionally ignore a nonce
  or tool instruction. Mitigation: use trivial fixed prompts and forced tool choice when
  supported, never compare prose, do not hide semantic failures with retries, and
  inspect trends before changing assertions.
- **Rate limits and outages:** An exhaustive run can be throttled. Mitigation: serial
  provider-local defaults, production retry hints, isolated provider jobs, explicit
  blocked states, and manual reruns without relabeling failures as passes.
- **Catalog ambiguity:** Some catalogs do not expose endpoint/tool capabilities.
  Mitigation: bounded live probes, documented structured classifications, and `unknown`
  fail-closed outcomes instead of name heuristics.
- **Meta preview and regional access:** The public service or catalog contract may be
  unavailable to the runner. Mitigation: require an explicit verified endpoint and
  supported-region account/runner; keep the aggregate result blocked until both exist.
- **Private model metadata exposure:** Account catalogs can include customer-owned IDs.
  Mitigation: dedicated empty test accounts where possible, pseudonymized artifacts,
  bounded in-memory mapping, and whole-artifact sensitive-value scans.
- **Live suite false authority:** A green run can encourage removal of fixtures.
  Mitigation: state evidence expiry, keep T022 gates release-authoritative, and prohibit
  live success as the sole protocol/safety proof.
- **OpenRouter list staleness:** A reviewed entry may disappear or change price.
  Mitigation: catalog validation, expiry dates, projected-cost ceilings, and hard failure
  rather than silent substitution.
- **Long workflow duration:** Full matrices can exceed hosted runner limits. Mitigation:
  independent provider jobs, bounded requests, resumable diagnosis through artifacts,
  and no claim of success for truncated jobs.

## Acceptance Criteria

### Discovery and Accounting

- [ ] OpenAI, Anthropic, Gemini, xAI/Grok, Meta, and OpenRouter catalog clients have
  bounded deterministic fixtures for success, pagination where applicable, repeated
  cursors/tokens, conflicting duplicates, malformed fields, HTTP-safe errors,
  cancellation, deadline, and response/item/page limits.
- [ ] A full direct-provider run accounts for every unique entry in the exact
  account-visible catalog snapshot; no entry disappears through filtering or a cap.
- [ ] Gemini executes every entry advertising `generateContent` and records all other
  entries as evidence-backed `not_applicable`.
- [ ] xAI canonical IDs execute once and aliases do not create duplicate paid calls.
- [ ] OpenAI and Meta capability gaps are resolved by bounded probes, never model-name
  prefixes or remote free-form error text.
- [ ] Private/fine-tuned visible models are assessed and pseudonymized in artifacts.
- [ ] Catalog drift between discovery and generation is explicit and makes the run
  incomplete.

### OpenRouter Restriction

- [ ] `openrouter_models.toml` has a strict tested schema with exact canonical IDs,
  transports, required scenarios/capabilities, rationale, owner, review/expiry dates,
  and optional cost ceilings.
- [ ] The initial reviewed allowlist is validated against the live catalog and contains
  no wildcard, family, auto-router, latest, duplicate, expired, or silently normalized
  entry.
- [ ] Every allowlist entry passes its required live scenarios; missing or incompatible
  entries fail the job.
- [ ] Newly discovered non-allowlisted OpenRouter models are reported but never
  automatically executed or added.

### Production-Path Qualification

- [ ] Every generation request uses the production provider registry, configuration
  resolution, factory, typed request/response contract, adapter, stream decoder, retry,
  safe terminal mapping, redaction, and native continuation path.
- [ ] Every eligible profile passes complete text and streamed text with distinct nonce
  proofs, bounded output, and validated private terminal authority.
- [ ] Every tool-capable profile passes one inert structured tool call, exact neutral
  result correlation, native continuation, and final receipt without invoking
  ToolExecutor.
- [ ] Profiles claiming parallel tools pass the two-call correlation scenario.
- [ ] Every advertised model exists or resolves through documented canonical metadata,
  qualifies on its default transport, and passes the agent tool profile.
- [ ] No live assertion depends on exact prose, cross-request content equality, model
  quality, latency ranking, or a provider-supplied free-form error message.

### Safety, Bounds, and Reporting

- [ ] Local catalog execution requires explicit live-network acknowledgement; local
  canary/selected/full execution additionally requires paid-cost acknowledgement. Ordinary
  `task check`, `task test`, coverage, PR, and fork workflows make zero live LLM calls.
- [ ] Dedicated test credentials, safe endpoint validation, request/token/attempt/
  concurrency/deadline/cost ceilings, and no-semantic-retry behavior are enforced.
- [ ] A budget, auth, quota, billing, region, rate-limit, catalog, or configuration
  blocker cannot be classified as model incompatibility or a passing skip.
- [ ] Versioned JSON and Markdown reports are complete, bounded, atomically published,
  and contain no raw bodies, remote messages, headers, prompts, nonces, reasoning,
  private provider state, or private raw model IDs.
- [ ] Raw and encoded credentials plus configured sensitive endpoint/model values are
  absent from test output, logs, reports, persisted session state, and uploaded
  artifacts; a detected value suppresses upload and fails the run.
- [ ] Partial, killed, timed-out, and artifact-truncated runs cannot report success.

### Automation and Evidence

- [ ] The five documented Task targets exist, are stable, and are described in
  `docs/tech/task.md`.
- [ ] `selected_models.toml` strictly defines all six providers, exact model IDs, the
  three mandatory nib task scenarios, conditional parallel-tool coverage, ownership,
  review/expiry dates, and no wildcard or implicit fallback.
- [ ] Selected mode fails before generation when the matrix is invalid/expired, a
  selected model is absent from the live catalog, an OpenRouter selection is not
  separately approved, or the complete request budget cannot cover the matrix.
- [ ] Selected reports include the suite ID and matrix fingerprint; the aggregate job
  rejects missing/mismatched provenance and reports every provider/model/task result.
- [ ] The protected manual/scheduled workflow uses isolated provider jobs, fail-fast
  disabled, read-only repository permissions, environment secrets, concurrency and
  timeout bounds, no PR/fork trigger, sanitized short-retention artifacts, and a strict
  aggregate result.
- [ ] Catalog, canary, and full modes have deterministic dry-run tests that prove their
  planned denominators and maximum request/token/attempt counts before I/O.
- [ ] One exact implementation revision has a complete full pass for OpenAI, Anthropic,
  Gemini, xAI/Grok, Meta, and the OpenRouter reviewed allowlist.
- [ ] Exact provider/model/catalog hashes, timestamps, source revision, runner platform,
  scenario counts, and bounded cost/usage evidence are recorded in this spec before it
  moves to done.
- [ ] `task check`, `task test`, `task docs:check`, `task check:all-targets`,
  `task coverage`, `task build`, and `git diff --check` pass on the exact implementation
  revision.
- [ ] Independent spec-compliance and technical/security reviews have no unresolved
  blocker.

## Affected Areas

- `tests/llm_live.rs` and live-only support modules/fixtures
- `tests/fixtures/llm_live/openrouter_models.toml`
- `tests/fixtures/llm_live/selected_models.toml`
- provider catalog fixtures and report/redaction fixtures
- `src/llm/registry.rs`, `src/llm/factory.rs`, and T022 provider interfaces only where
  minimal testable catalog/override hooks are required
- `Cargo.toml` ignored-test/feature configuration
- `Taskfile.yml`
- `.github/workflows/llm-live.yml`
- `docs/tech/task.md`, `docs/tech/ci.md`, `docs/tech/backend_rust.md`,
  `docs/user/guide.md`, and provider support documentation
- T021/T022 implementation evidence and `docs/specs/README.md`

## Validation Gates

- Deterministic catalog and pagination fixtures for all six providers
- Deterministic model/profile classification and completeness matrices
- Deterministic OpenRouter allowlist schema/catalog reconciliation tests
- Deterministic selected-suite schema, exact-model/catalog, scenario, expiry, and
  fingerprint tests
- Deterministic dry-run budget/request/token/attempt planning tests
- Deterministic report bounds, pseudonymization, atomic-write, redaction, and artifact
  suppression tests
- Prohibition tests proving ordinary Task/CI targets cannot select live tests
- `task test:llm-live:offline`
- Manual catalog and canary runs for each provider
- Exact-revision full live matrix for every direct-provider catalog plus the OpenRouter
  allowlist
- `task check`
- `task test`
- `task docs:check`
- `task check:all-targets`
- `task coverage`
- `task build`
- `git diff --check`
- Independent spec-compliance review followed by technical/security review

## Implementation Reconciliation (2026-08-06)

The first implementation slice is present and intentionally remains in development:

- `tests/llm_live.rs` is ignored for network execution while its catalog decoders,
  planner, allowlist schema, reporting, redaction, and scenario helpers are exercised by
  ordinary credential-free tests.
- Catalog discovery is implemented for the six provider entries with HTTPS-only roots,
  redirects disabled, bounded bodies/items/pages/deadlines, Anthropic and Gemini
  pagination, canonical-ID deduplication, and safe status-only failures.
- Canary/full plans are computed before generation with exact logical-request,
  maximum-attempt, output-token, and projected-cost bounds. Full direct-provider plans
  fail when a bundled advertised ID or alias is missing. OpenRouter joins only exact
  approved allowlist IDs; non-allowlisted catalog entries remain accounted for but are
  never executed.
- Completion, streaming, single-tool continuation, and parallel-tool continuation use
  synthetic nonces and the production provider registry, `NibConfig` validation,
  diagnostics, factory, adapters, stream terminal envelope, and private continuation.
  The inert qualification tools are never registered with or passed to `ToolExecutor`.
- `LlmRequest::with_max_output_tokens` now maps to each native provider transport so
  live output is bounded without changing existing callers that omit the option.
- Reports are versioned, bounded, pseudonymize private catalog IDs, record catalog
  hashes and plan ceilings, detect post-generation catalog drift, scan raw and encoded
  configured secrets, and publish atomically without replacing prior evidence.
- The original three explicit Task targets and the separate protected GitHub Actions workflow
  are implemented. The workflow has no pull-request trigger, scopes one credential to
  one provider execution step, disables fail-fast, uploads only harness-published
  sanitized artifacts (including failed semantic evidence), and strictly aggregates
  their JSON results. Paid scheduling remains
  disabled.

The selected-suite slice added on 2026-08-17 also remains in development:

- `task test:llm-live:selected` validates the strict external
  `selected_models.toml` matrix and executes the three core nib tasks on every exact
  configured model/production transport, with conditional parallel-tool coverage;
- `task test:llm-live:offline` provides a credential-free focused gate for the parsers,
  matrix, planner, reports, and protected workflow contract;
- the initial matrix selects one bundled default for every direct provider and four
  OpenRouter family representatives, and records ownership plus review/expiry dates;
- exact live catalog membership is mandatory and selected OpenRouter IDs remain subject
  to the separate approved allowlist, so current unapproved entries still fail closed;
- JSON report schema 2 and Markdown summaries record suite/fingerprint/task provenance,
  while the aggregate workflow rejects mismatched fingerprints and lists each selected
  model/transport/task result; and
- the paid Wednesday selected schedule is externally enableable only through
  `NIB_LIVE_SCHEDULE_MODE=selected` after the existing manual-pass, credential, budget,
  Meta endpoint, and OpenRouter approval gates. Catalog remains the default.

Credential-free validation for this slice on 2026-08-17:

- `task test:llm-live:offline` passed 32 tests with the live network entrypoint ignored;
- `task docs:check`, `task check:all-targets`, `task build`, strict TOML/YAML parsing,
  the selected paid-cost precondition, and `git diff --check` passed;
- An earlier `task test` attempt reached 734 passing library tests before two assertions
  in the concurrently developed T026 scheduled-provider-failure reporting slice failed;
  those assertions were subsequently corrected.
- An earlier `task check` attempt reached three T026-only Clippy findings in
  `src/llm/error.rs` and `src/llm/mod.rs`; those findings were corrected without lint
  suppression, and the combined-tree `task check` passed on 2026-08-17. No
  selected-suite lint finding was emitted.

Credential-free validation completed on 2026-08-06:

- `task check`, `task test`, `task docs:check`, and `task check:all-targets` passed;
- `task coverage` passed with 83.98% runtime line coverage (63,244 / 75,310 lines). The
  command required the repository's normal unrestricted test sandbox because its
  pre-existing HTTP and special-file fixtures cannot run inside a network-denied
  filesystem sandbox;
- `task build` completed the locked optimized release build;
- `git diff --check` and a strict YAML parse of `.github/workflows/llm-live.yml` passed;
  and
- both catalog-without-network-acknowledgement and canary-without-cost-acknowledgement
  Task invocations failed at their preconditions before Cargo or network execution.

Completion evidence is still blocked on external and prerequisite work, so no unchecked
acceptance item is being claimed prematurely:

- no provider credentials, dedicated budget-capped accounts, or supported-region Meta
  endpoint were available in this workspace, so no catalog/canary/selected/full live result was
  generated;
- every initial OpenRouter entry is `approved = false` pending an authenticated catalog,
  capability, regional-availability, and price review; paid OpenRouter modes therefore
  fail closed; and
- T022 still lacks typed provider error/usage/actual-attempt metadata and the complete
  deterministic conformance matrix required for release-authoritative evidence. The
  harness consequently keeps ambiguous model rejections as adapter failures rather
  than inferring unsupported capability from remote prose.

## Open Questions

- Which exact current OpenRouter models should replace any advertised entries that fail
  canonical catalog validation? The policy and schema are decided here, but the initial
  IDs require a live catalog snapshot and human support/cost review.
- Which dedicated provider projects/accounts, hard spend limits, and supported-region
  runner will own scheduled credentials, especially for the Meta public preview?
- What request and projected-cost ceilings fit the first observed full catalog sizes?
  Implementation must collect catalog/dry-run counts before enabling paid schedules;
  incomplete lower ceilings remain a failure, not reduced coverage.

## External References

Verified on 2026-08-06:

- [OpenAI Models API](https://platform.openai.com/docs/api-reference/models)
- [Anthropic Models API](https://platform.claude.com/docs/en/api/models/list)
- [Gemini Models API](https://ai.google.dev/api/models)
- [xAI language-model catalog](https://docs.x.ai/developers/rest-api-reference/inference/models)
- [OpenRouter Models API](https://openrouter.ai/docs/api/api-reference/models/get-models)
- [Meta Model API public-preview announcement](https://ai.meta.com/blog/introducing-muse-spark-meta-model-api/)

The Meta reference establishes the direct public-preview service and OpenAI-compatible
developer surface, but not yet a stable catalog endpoint in the accessible public
reference. The explicit-base-URL blocker above is intentional until implementation can
record authoritative catalog documentation and a bounded fixture.
