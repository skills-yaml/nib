# T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility

**Status:** Development

**Related:**
[FT-004: LLM Integration and Agent Loop](../done/ft_004_llm_integration_and_agent_loop.md),
[FT-011: LLM Streaming and TUI](../done/ft_011_llm_streaming_and_tui.md),
[T007: Configuration and Doctor](../development/T007_configuration_schema_alignment_and_nib_doctor_validation.md),
[T010: Release Process](../done/T010_release_process.md)

## Summary

Add an explicit API-mode and reasoning contract to nib's Rust OpenAI-compatible
providers. Official OpenAI reasoning models must be able to use function tools through
the Responses API, while Chat Completions remains available for compatible endpoints.
nib must never silently disable reasoning, infer capabilities from a model-name prefix,
or retry a rejected request through a different API mode.

## Incident Analysis (2026-07-29)

### Observed Failure

The reported run selected `openai` with `gpt-5.6-luna` and failed before the first
agent step completed:

```text
Function tools with reasoning_effort are not supported for gpt-5.6-luna in
/v1/chat/completions. To use function tools, use /v1/responses or set
reasoning_effort to 'none'.
```

The installed binary reported `nib 0.1.0 (prod - 1afeed8...)`. Its chat UI printed
`delegating to Python LLM + tools`, but that text was stale: the same function invoked
the in-process Rust agent loop. Current source has removed the label, and nib has no
Python or LiteLLM execution path.

### Pre-Remediation Request Path

The following describes the reported binary and source baseline
`b2c6586062e9833ca62b0aff4faf145f128e8c8d`, before the T021 implementation:

- Planning always supplies the `submit_plan` function tool, so this combination is
  reached before optional execution tools are selected.
- Baseline `src/llm/openai.rs` always constructed Chat Completions payloads and normalized every
  configured OpenAI-compatible base URL to `/chat/completions`.
- The same client was used for OpenAI, Grok, OpenRouter, and Meta.
- Baseline `ProviderEntry` exposed only the model, credentials, and base URL. It had no API mode
  or reasoning-effort setting.
- Neither the installed binary nor the baseline source serialized `reasoning_effort`. The
  remote endpoint or an upstream compatibility layer therefore applied the effective
  non-`none` reasoning value.
- Pointing `base_url` at `/v1/responses` was not a workaround because the baseline URL
  builder appended `/chat/completions`.

### Root Cause

The immediate failure was an unsupported request tuple: Chat Completions, function
tools, and effective non-`none` reasoning. The durable nib defect was broader: API
dialect and reasoning were implicit provider behavior, while the shared adapter assumed
that every OpenAI-compatible endpoint implemented the Chat Completions request,
streaming, and tool-call contract.

The stale release and false Python label made diagnosis harder, but updating only that
label or binary alone would not have resolved the protocol gap because the incident
source baseline was Chat-Completions-only.

## Decision

Implement typed `chat_completions` and `responses` API modes plus an optional typed
reasoning effort. Add a complete Responses request/response and SSE adapter for custom
function tools. New official OpenAI configurations use Responses; existing unversioned
provider entries retain Chat Completions until the operator explicitly migrates them.

No runtime path may silently change a requested reasoning effort, infer API support
from model-name substrings, or resend a rejected request with changed semantics.

## Goals

- Run the structured planner and normal tool loop with OpenAI reasoning models through
  `/v1/responses`.
- Make the resolved API mode, endpoint, and reasoning setting explicit and locally
  diagnosable without a paid request.
- Preserve existing Chat Completions behavior for Grok, OpenRouter, Meta, and custom
  compatible endpoints unless explicitly configured otherwise.
- Normalize both API dialects into the existing provider-neutral agent stream while
  preserving function-call identity through tool-result continuation.
- Keep provider failures bounded, redacted, auditable, and correctly reconciled with
  the authoritative session workload.

## Non-Goals

- Restoring the historical Python or LiteLLM runtime.
- Discovering provider capabilities by issuing paid probe requests.
- Maintaining a hard-coded catalog keyed by model-name prefixes.
- Adding OpenAI-hosted web, file, shell, computer-use, or MCP tools.
- Persisting raw chain-of-thought, encrypted reasoning payloads, credentials, or
  provider response bodies in session audit records.
- Replacing the existing approval, ToolExecutor, worktree, or reconciliation model.
- Making live paid-provider calls a mandatory CI gate.

## Scope

### Provider Configuration

Extend OpenAI-compatible provider entries with:

```toml
[llm.providers.openai]
model = "gpt-5.6-luna"
api = "responses"              # responses | chat_completions
reasoning_effort = "medium"    # optional: none|minimal|low|medium|high|xhigh|max
```

The fields are typed, bounded, round-trippable, and redacted-safe. Unknown values fail
configuration validation. Providers whose adapters do not consume these fields reject
them instead of silently ignoring them.

Compatibility policy:

- Existing entries without `api` continue to resolve to `chat_completions`.
- Newly created OpenAI entries that use nib's canonical OpenAI endpoint use
  `responses`.
- Newly created Grok, OpenRouter, and Meta entries use `chat_completions`.
- An explicit value always wins; no model-name heuristic changes it.
- A provider named `openai` with a custom `base_url` is a custom gateway, not proof of
  official OpenAI capabilities. It must select its API mode explicitly or retain the
  legacy Chat Completions default.
- A configured root URL gets exactly one suffix for the selected API mode. A full
  endpoint is accepted only when it matches the selected mode. Conflicting or doubled
  suffixes fail before network I/O.

`reasoning_effort` is omitted when not configured. The enum validates syntax, not a
mutable provider/model capability matrix. A structurally valid Chat Completions request
with tools and configured reasoning is sent as configured; nib does not reject it from
the provider name or a model-name heuristic. If the provider rejects the combination,
the bounded 4xx diagnostic names the explicit `responses` and `none` remedies without
automatically choosing either. `nib doctor` warns that tool/reasoning compatibility is
provider- and model-dependent when Chat Completions is selected. For nib's canonical
OpenAI endpoint it recommends considering Responses for tool/reasoning workflows, but
does not inspect model names or claim that every OpenAI Chat model has the same
limitation.

### Provider-Neutral Request Contract

Replace the growing positional `LlmClient` request surface with a structured request
object containing messages, tools, temperature, API-neutral reasoning options, and an
optional active-turn continuation. Planning, compression, chat, one-shot runs, TUI,
MCP, and delegated agent loops use the same request builder and validation path.

Both completion modes yield a provider-neutral completed-turn envelope containing
content, terminal status, tool calls with typed provider call IDs, and an optional
provider continuation. Streaming returns a private provider-stream handle: the agent
loop can finish it but cannot receive raw provider deltas. After terminal validation,
the loop derives sanitized content and tool events only from the completed envelope.
Partial provider events cannot authorize tool execution or reach a public observer. The continuation
is an ordered, byte/item-bounded provider value bound to the originating provider,
model, API mode, session, and run. It is returned explicitly to the loop, consumed only
by the matching next request, and never becomes mutable state hidden inside a shared
client. Provider call IDs and continuation items never enter CLI/TUI observer channels;
their debug and serialization behavior is redacted by construction.

### Responses Adapter

Implement Responses as a separate dialect within `src/llm/`:

- Send `input` instead of `messages` and flatten Chat-style function definitions into
  Responses function tools.
- Send `reasoning: { effort: ... }` only when configured and `store: false` on every
  supported Responses request to preserve nib's local-first persistence contract.
- Do not copy Chat-only parameters into Responses requests without fixture evidence.
  In particular, temperature is omitted when a configured reasoning mode makes its
  support ambiguous.
- Parse output text, refusal/error items, multiple function calls, fragmented function
  arguments, terminal status, and usage-independent completion.
- Preserve every function `call_id` and the complete ordered `response.output` sequence
  required by a stateless active turn, including original reasoning and `function_call`
  items. After ToolExecutor returns, replay those bounded items and append each matching
  `function_call_output`; request and replay encrypted reasoning content when the
  selected model contract requires it.
- Keep opaque reasoning/continuation items in memory only for the active run. Persist
  normalized tool intent, approvals, results, errors, and reconciliation evidence in
  the existing provider-neutral session format.
- Parse Responses SSE event types inside the private provider stream and return one
  private completed-turn envelope while retaining current byte limits, early
  receiver-drop behavior, cancellation, retry policy, and credential rotation. Public
  content/tool events are projected from that envelope only after validation.

`store: false` prevents nib from asking the API to retain application response state;
it is not presented as a Zero Data Retention guarantee. User documentation must keep
provider-side data handling distinct from nib's local persistence contract.

### Chat Completions Adapter

Keep the existing Chat Completions payload and SSE parser as a first-class mode. It
must not receive Responses-only fields or flattened Responses tool schemas. Explicit
reasoning effort is sent where configured. Tool/reasoning support remains a model and
endpoint capability; an actual structured 4xx follows the diagnostic policy above and
is never retried through a different mode or effort.

### Crash And Restart Semantics

Opaque continuation is deliberately not durable. If nib stops after a tool result is
persisted but before the next model response, recovery recognizes the completed tool
record, never executes that tool again, and reconciles the interrupted run through the
existing terminal failure path. A later user-started run may use normalized audit
evidence as ordinary context, but it is a new provider turn and cannot claim to resume
the discarded opaque continuation.

### Diagnostics And Failure Semantics

`nib doctor` and redacted config display report:

- the resolved configuration source,
- provider and model,
- resolved API mode and endpoint path,
- configured reasoning effort or `provider_default`, and
- a local warning for provider/model-dependent combinations or a failure when the
  configuration is structurally invalid.

Diagnostics never print credentials, query strings, opaque reasoning, full prompts, or
unbounded provider bodies. Semantic and other non-transient 4xx responses remain
non-retryable and bounded. Existing transient `408`, `425`, and `429` handling may
retry the identical request, but never changes API mode, reasoning effort, or other
request semantics. Errors identify the provider, model, API mode, and safe operator
actions.

Planning or model-stream failure occurs before any unapproved tool executes. The agent
records the failure reason and reaches normal reconciliation; it must not leave a plan
step or workload record falsely running.

### Release Provenance

The corrected behavior is not complete until a release binary contains it. T010 owns
channel publication, but T021 validation records the tested build SHA and verifies that
the installed binary reports that SHA. The obsolete Python-delegation label must not
appear in release smoke output.

## Alternatives Considered

### Always Force `reasoning_effort = "none"`

Rejected as the primary fix. It restores compatibility by silently discarding a
requested model capability and changes quality, latency, and cost semantics.

### Retry The 400 Through Responses Or With Reasoning Disabled

Rejected. Automatic semantic retries can duplicate provider work, obscure the
effective configuration, complicate audit evidence, and produce different behavior
after an operator explicitly selected a mode.

### Infer Capability From `gpt-5.6-*`

Rejected. Model aliases and provider routing change independently of nib releases, and
third-party endpoints can expose the same name with different behavior.

### Move Every OpenAI-Compatible Provider To Responses

Rejected. OpenRouter, xAI, Meta, and custom gateways do not share one guaranteed
Responses contract. They retain Chat Completions defaults and may opt into Responses
only through explicit configuration and fixture-proven compatibility.

### Restore LiteLLM Or A Python Bridge

Rejected. It would violate the pure-Rust architecture, add a second execution/runtime
boundary, and would not remove the need for explicit capability and audit semantics.

## Risks And Tradeoffs

- Responses uses typed output items rather than Chat choices, increasing parser and
  streaming state complexity. Mitigation: separate dialect parsers and credential-free
  fixtures for every event transition.
- Active-turn continuation can contain provider-opaque data. Mitigation: strict byte
  bounds, in-memory-only lifetime, no debug rendering, and explicit redaction tests.
- Preserving Chat mode for legacy entries means an existing GPT-5.6 configuration may
  still receive one actionable incompatibility error before migration. Mitigation:
  doctor diagnostics, release notes, and new OpenAI auth defaults.
- Switching API mode can change model behavior, latency, and token use. Mitigation: no
  automatic fallback, explicit configuration, and side-by-side deterministic fixtures.
- Custom gateways may partially implement Responses. Mitigation: explicit opt-in,
  bounded protocol errors, and no paid capability probe.
- A release can lag source after implementation. Mitigation: bind acceptance evidence
  to an installed build SHA and T010 publication evidence.

## Implementation Plan

1. Capture provenance and reproduce the rejected tuple with a credential-free HTTP
   fixture before changing request construction.
2. Add typed API-mode and reasoning configuration, backward loading, auth defaults,
   endpoint validation, redacted display, and doctor diagnostics.
3. Introduce structured provider request/completed-turn types, typed call identity, and
   bounded active-turn continuation without changing ToolExecutor or session authority.
4. Implement non-streaming and streaming Responses request construction, tool schema
   conversion, typed event parsing, call-ID continuation, and `store: false` behavior.
5. Preserve and expand Chat Completions regression coverage for all compatible
   providers and custom URLs.
6. Add planner, runtime tool, failure-reconciliation, CLI/TUI, and release-binary smoke
   coverage; update user and technical documentation.
7. Run the canonical gates and publish through T010 before advertising the fix as
   available to installed users.

## Rollout Plan

- Phase 1 adds the explicit fields and Responses support while legacy entries continue
  using Chat Completions.
- Phase 2 makes `nib auth` write `api = "responses"` for new official OpenAI entries and
  documents the migration for existing users.
- Phase 3 validates an exact release binary and channel artifact. A future spec may
  change the legacy default only after compatibility and upgrade evidence exists.

No automatic configuration rewrite changes an existing provider's API mode or
reasoning effort.

T027 adds one explicit preflight exception to this rollout policy: an operator may run
`nib doctor --fix` to migrate an eligible canonical OpenAI Chat Completions entry to
Responses. Ordinary doctor and runtime execution still never rewrite configuration,
no rejected request is retried, custom gateways remain excluded, and the selected
model and reasoning effort are preserved. This reconciles the existing no-fallback
rule with a deterministic operator-requested repair.

## Acceptance Criteria

- [x] A credential-free fixture reproduces the reported Chat Completions tools plus
  effective reasoning rejection and records the exact request path/body without
  secrets.
- [x] Provenance diagnostics distinguish the running binary SHA, resolved config
  source, and Rust execution path; no UI claims delegation to Python.
- [x] Provider config round-trips typed `api` and `reasoning_effort`, rejects unknown
  values, fields unused by a provider, and conflicting endpoint suffixes, and loads
  legacy TOML with Chat Completions behavior unchanged.
- [x] New official OpenAI auth config selects Responses; Grok, OpenRouter, Meta, and
  legacy entries retain Chat Completions defaults.
- [x] Responses completion posts exactly `/responses`, uses `input`, flattened function
  schemas, configured `reasoning.effort`, and `store: false`.
- [x] Responses streaming reconstructs text and multiple fragmented function calls,
  yields one private completed-turn envelope, closes promptly when the consumer drops,
  and enforces the existing response/event byte limits.
- [x] A planner fixture completes `submit_plan` over Responses, and a runtime fixture
  returns each tool result with the matching `call_id` before producing final text; the
  next request replays the complete required ordered output sequence.
- [x] Complete and streaming paths return bounded continuation explicitly, reject a
  continuation used with another provider/model/API/session/run, and prove concurrent
  sessions cannot observe or consume each other's call IDs or opaque items.
- [x] CLI, TUI, MCP, and other public observers receive only terminal-authoritative,
  sanitized projected events and cannot receive provider call IDs, encrypted
  reasoning, opaque continuation items, or preterminal provider deltas.
- [x] Planner and runtime provider failures execute no unauthorized tool, persist a
  bounded redacted failure, and reconcile the session/workload to a truthful terminal
  state.
- [x] Chat Completions fixtures prove existing text/tool behavior and ensure no
  Responses-only field reaches compatible providers.
- [x] Chat fixtures cover a canonical OpenAI endpoint, `provider = "openai"` with a
  custom gateway, and older/unknown models. Valid requests are not rejected by name;
  an actual tools/reasoning 4xx instructs the operator to select Responses or explicitly
  choose `none`, with no semantic retry or downgrade.
- [x] Provider 4xx errors are bounded and redacted, identify model/API context without
  exposing prompts or credentials, and do not receive semantic retries; non-transient
  4xx responses are not retried, while `408`, `425`, and `429` may retry only the
  identical request under the existing transient-status policy.
- [x] `nib doctor` reports effective provider/API/reasoning configuration without a
  paid provider call.
- [x] A kill/restart fixture between persisted tool completion and model continuation
  reconciles the interrupted run terminally and proves the completed tool is not
  executed twice.
- [ ] An exact release binary exercises help, version, doctor, structured planning,
  one Responses tool round trip through a credential-free local fixture, and failure
  reconciliation; its reported SHA matches the validated artifact.
- [x] User and technical documentation explain API-mode selection, migration, privacy,
  and the absence of automatic semantic fallback.

## Implementation Evidence (2026-07-29)

Source implementation and credential-free fixtures satisfy every acceptance criterion
except the exact committed release-artifact criterion. Independent spec-compliance and
technical reviews found no remaining source-code blockers.

- `task fix`, `task check`, `task test`, `task docs:check`,
  `task check:all-targets`, and `task build` pass.
- `task check` and `task test` pass 667 library tests, 70 binary tests, every
  integration group, and 15 runtime E2E tests. The runtime suite includes a real nib
  process kill/restart after durable tool completion and proves no duplicate execution.
- `task coverage` passes at 84.19% runtime line coverage (61,044 / 72,511).
- `git diff --check` passes.
- The local optimized binary passes help, doctor, and mock structured-planning smoke.
  Doctor reports the native Rust runtime and resolved provider transport without a
  network provider call.
- The local build reports `b2c6586062e9833ca62b0aff4faf145f128e8c8d`, the current
  base commit, because these changes are not committed. The installed production nib
  still reports `1afeed82a0f95ed70c1d9a5b31221a38a1830c78`.

The unchecked release criterion remains blocked on committing the implementation,
running the complete release-binary smoke against that exact SHA, and obtaining native
Linux, macOS, and Windows CI evidence. T021 therefore remains in Development.

## Release-Binary Harness Progress (2026-08-23)

The remaining release path now has a repository-owned, credential-free qualification
target without claiming that this uncommitted worktree is the required artifact:

- `task qualify:llm-release` injects the checkout's full `git rev-parse HEAD` into the
  locked optimized build and fails if `nib version` reports any other embedded commit.
- The resulting executable exercises `--help`, `--version`, `version`, and `doctor`,
  then runs structured planning and one correlated Responses tool-result round trip
  through bounded `127.0.0.1` fixtures. A second fixture proves a typed, redacted
  planning failure reaches terminal session reconciliation.
- The harness computes the executable SHA-256 and writes bounded JSON evidence to
  `target/release-qualification/t021-release-binary.json`, including platform,
  architecture, source revision, embedded revision, and the exercised path names.
- Evidence records whether the source worktree was clean and sets
  `acceptance_eligible` to that value. The current dirty source can validate harness
  behavior but cannot satisfy the unchecked exact committed artifact criterion.
- Deterministic tests enforce the Task contract and exact build-identity parser. The
  full ignored release-binary test runs only through the explicit Task target and does
  not read provider environment credentials or make a non-local network request.

The acceptance checkbox remains unchecked until this target passes for the clean,
committed implementation revision on the required native release artifacts/CI hosts.

## Native CI Qualification Wiring (2026-09-02)

The Validate, Windows Tests, and macOS Tests jobs now run
`task qualify:llm-release` in place of a build-only step, before their native smoke.
Static installer coverage asserts that all three jobs retain that exact qualification
boundary. A local dirty-tree run passed help, embedded version, doctor, structured
planning, Responses tool continuation, typed failure reconciliation, and executable
stability for source revision `8803408240d4c00ebc4027041c073c7f540360cc`; its
evidence correctly reports `source_worktree_clean = false` and is not acceptance
eligible. The checkbox remains open until the pushed clean revision produces native
hosted evidence.

## Local Release-Artifact Qualification (2026-08-26)

`task qualify:llm-release` passed against the locked optimized Linux binary. The
resulting bounded evidence records source revision
`10389e0b61cf097b4a48f26afbbae75c632f0a1f`, executable SHA-256
`54bd156293a512e179aa4675c601b440292c7cb350981555b474d43eb2daa5bd`, and successful
help, embedded-version, doctor, structured-planning, Responses tool-result, and typed
failure-reconciliation exercises. The sequential success/failure fixture now reloads
the committed configuration revision before its second mutation, so the harness also
honors the production stale-snapshot guard.

This evidence deliberately reports `source_worktree_clean = false` and
`acceptance_eligible = false`. It proves the local harness and exact built executable,
not the required clean committed cross-platform release artifact; the acceptance
checkbox and Development state therefore remain unchanged.

## Windows Task-Contract Portability (2026-09-02)

Hosted Windows run `33665599019` passed the complete test suite through the native Job
Object, MCP lifecycle, and credential-free LLM report-publication coverage, then exposed
that the deterministic release Task contract test parsed only LF-delimited YAML. Windows
checks out the Taskfile with CRLF delimiters, so the test now normalizes CRLF to LF before
inspecting the same exact target and required commands. The Task target and production
release qualification behavior are unchanged.

## Affected Areas

- `src/config/mod.rs`, `src/auth.rs`, `src/config_cmd.rs`, and `src/doctor.rs`
- `src/llm/mod.rs`, `src/llm/openai.rs`, `src/llm/factory.rs`, `src/llm/types.rs`, and
  potentially a new `src/llm/responses.rs`
- `src/llm/anthropic.rs`, `src/llm/gemini.rs`, and `src/llm/mock.rs` for the shared
  request/completed-turn contract and regression coverage
- `src/agent/planner.rs` and `src/agent/loop.rs` for structured requests and active-turn
  call identity
- `src/agent/state.rs`, `src/session/`, and `src/daemons/workload.rs` for interrupted-run
  recovery and authoritative no-reexecution reconciliation
- `src/context/compression.rs` and every other direct `LlmClient` caller
- `src/chat.rs`, `src/run.rs`, and `src/tui/` diagnostics and request plumbing
- Provider/config/doctor/runtime integration fixtures
- `README.md`, `docs/user/guide.md`, `docs/tech/backend_rust.md`, and
  `docs/tech/architecture.md`
- T010 release evidence and release-binary smoke

## Validation Gates

- Focused config migration, endpoint resolution, and doctor/redaction tests
- OpenAI-compatible Chat Completions complete/SSE fixtures
- Responses complete/SSE text, tool, ordered continuation, observer isolation, error,
  size, and disconnect fixtures
- Structured planner and runtime tool-call E2E with authoritative session reconciliation
- Kill/restart coverage after persisted tool completion with no duplicate execution
- `task fix`
- `task test`
- `task check`
- `task docs:check`
- `task coverage`
- `task build`
- Release-binary smoke tied to the exact implementation SHA
- Native Linux, macOS, and Windows CI for the exact implementation SHA
- `git diff --check`

## Open Questions

- After how many successful releases should official OpenAI legacy entries without an
  explicit `api` field be eligible for a separately specified default migration?
- Which third-party providers should receive dedicated Responses fixtures before their
  auth defaults may offer that mode?

## Hosted CI Reconciliation (2026-08-02)

The first merged T021/T022 revision exposed four Clippy diagnostics that local evidence
had not retained. The remediation uses an explicit conditional for response guidance,
documents the validated continuation constructor's intentionally independent security
boundaries, and uses direct membership lookup for pending invocation IDs. OpenAI tool
completion and stream fixtures now provide the required request scope before a tool
continuation can be created.

These changes do not alter transport selection or continuation authority. The exact
local `task check` gate passes 689 library tests, 77 CLI tests, and every integration
group. Hosted Linux, macOS, and Windows gates remain required before T021 can move to
done.

## External References

- [OpenAI model guidance](https://developers.openai.com/api/docs/guides/latest-model)
  recommends Responses for GPT-5.6 reasoning, tool-calling, and multi-turn workflows.
- [OpenAI Responses migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses)
  documents typed output items, function schema/call-ID differences, streaming events,
  and common migration errors.
- [OpenAI function-calling guide](https://developers.openai.com/api/docs/guides/function-calling)
  documents carrying response output and matching `call_id` values across tool turns.
- [OpenAI GPT-5.1 model reference](https://developers.openai.com/api/docs/models/gpt-5.1)
  demonstrates why Chat Completions, reasoning, and function support cannot be rejected
  globally from the provider name alone.
- [T027: Doctor-Guided OpenAI Transport Repair](../done/T027_doctor_guided_openai_transport_repair.md)
  owns the explicit configuration repair while preserving this spec's runtime
  no-fallback contract.
