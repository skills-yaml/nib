# T022: Provider-Neutral LLM Contract and Adapter Conformance

**Status:** Development

**Related:**
[FT-004: LLM Integration and Agent Loop](../done/ft_004_llm_integration_and_agent_loop.md),
[FT-011: LLM Streaming and TUI](../done/ft_011_llm_streaming_and_tui.md),
[T007: Configuration Schema Alignment](T007_configuration_schema_alignment_and_nib_doctor_validation.md),
[T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](T021_openai_compatible_reasoning_and_tool_transport_compatibility.md),
[Architecture](../../tech/architecture.md), and
[Backend Rust](../../tech/backend_rust.md)

## Summary

Replace nib's nominally provider-neutral LLM surface with a typed core contract and a
conforming implementation for every supported provider. Provider implementations may
share HTTP, SSE, Chat Completions, and Responses codecs, but each provider must own its
configuration, structural capabilities, request validation, native tool-result
continuation, terminal-state mapping, safe error decoding, and retry policy.

The result must make provider differences explicit without hard-coding mutable model
capabilities. A provider response can authorize tool execution only after its adapter
has produced a complete, validated, provider-neutral turn.

## Problem Statement

T021 fixes the reported OpenAI reasoning-plus-tools incident by introducing explicit
Chat Completions and Responses modes. That remediation also exposed broader contract
gaps which T021 does not own:

- `LlmRequest` carries raw `serde_json::Value` messages and OpenAI-shaped function
  definitions. Anthropic and Gemini silently coerce or drop values they cannot map.
- `ProviderContinuation` contains Responses-specific output items and
  `function_call_output` construction. Chat Completions, Anthropic, and Gemini do not
  return provider-native correlated tool results on the next request.
- OpenAI-compatible providers share one provider label and parser even where their
  in-band errors, endpoint support, retry hints, and terminal reasons differ.
- Anthropic and Gemini expose bounded but otherwise verbatim provider error bodies,
  while the OpenAI adapters omit provider-supplied diagnostic text.
- Chat, Anthropic, and Gemini do not consistently reject missing, truncated, blocked,
  refused, or in-band-error terminal responses before marking a turn complete.
- The generic provider config accepts values, including native-provider `base_url`,
  which some adapters silently ignore.
- Mock does not enforce the same unsupported-option contract as network providers, so
  deterministic tests can accept requests that production providers reject.
- Existing tests are organized by module, not as a shared conformance suite. They do
  not prove the same safety and tool-loop invariants for every registered provider.

These gaps make the workload model depend on wire-specific behavior. They also allow a
new provider or provider API change to bypass safeguards already implemented elsewhere.

## Decision

Keep one agent-facing `LlmProvider` port, but make its request, response, stream, and
error types provider-neutral and typed. Register one concrete provider implementation
for OpenAI, xAI/Grok, OpenRouter, Meta, Anthropic, Gemini, and Mock. Implementations may
delegate encoding and parsing to shared wire codecs; provider identity must remain a
first-class boundary around those codecs.

The contract will expose stable structural capabilities only. It will not infer
support from model-name prefixes or maintain a mutable model catalog. When support is
model- or gateway-dependent, explicit configuration selects the transport and the
provider may return a safe typed rejection. nib must not silently change API mode,
reasoning, tools, or any other request semantic before retrying.

## Relationship to T021

T021 continues to own the reported OpenAI Chat-versus-Responses incident, explicit
`api` and `reasoning_effort` configuration, canonical OpenAI Responses default,
Responses privacy/continuation requirements, and its exact release-binary smoke. T022
replaces the cross-provider request, continuation, terminal, error, retry, and factory
shape without weakening those decisions.

Release evidence must follow one of two explicit sequences:

1. T021 completes its exact-revision release gate before the first T022 runtime change,
   and T022 retains that revision as its regression baseline; or
2. if T022 runtime work begins first or shares the same change set, the complete T021
   release smoke is rerun against the final T022 implementation revision before either
   spec moves to done.

Evidence from an earlier binary cannot validate later T022 changes to the same provider
path. This documentation-only T022 activation does not itself invalidate T021 evidence.

## Goals

- Give planners, agent loops, compression, CLI, TUI, MCP, and delegated runs one typed
  provider-neutral request and completed-turn contract.
- Give every supported provider a distinct implementation boundary while reusing
  proven wire codecs and bounded transport helpers.
- Preserve native call identity when supplied, create sealed turn-bound correlation
  where it is not, and return provider-correct structured tool results for Responses,
  Chat Completions, Anthropic Messages, and Gemini GenerateContent.
- Reject unsupported or malformed request fields before network I/O instead of
  dropping or coercing them.
- Normalize provider completion, refusal, truncation, safety-block, and error states so
  only a validated completed turn can authorize a tool.
- Return bounded, redacted, control-safe typed errors without exposing provider bodies,
  prompt echoes, credentials, or opaque reasoning.
- Make endpoint, retry, and credential-rotation behavior explicit per provider.
- Apply one deterministic conformance suite to every provider implementation and both
  complete and streaming paths.
- Preserve existing configuration and session compatibility where behavior is valid.

## Non-Goals

- Adding new providers, multimodal input, hosted provider tools, web search, computer
  use, file APIs, or MCP tools exposed by an LLM provider.
- Adding Anthropic extended thinking, Gemini thinking levels, or any other new native
  reasoning feature. Native reasoning requires a separate capability and UX decision.
- Maintaining a hard-coded provider/model capability catalog or probing paid APIs to
  discover mutable capabilities.
- Guaranteeing that every model behind an arbitrary compatible gateway accepts tools
  plus reasoning. Unknown model-level compatibility remains a provider response.
- Silently disabling reasoning, removing tools, changing transport, or performing a
  semantic fallback after rejection.
- Persisting raw provider responses, raw provider errors, opaque reasoning, or active
  provider-turn state.
- Changing ToolExecutor approval, sandbox, worktree, or workload reconciliation
  authority.
- Replacing T021's OpenAI default migration, Responses implementation, incident
  evidence, or exact release criterion.
- Making live paid-provider calls mandatory in local or CI validation.

## Scope

### Core Invariants

The implementation must preserve these invariants across every provider:

1. Only a privately completed and validated provider turn may authorize tool calls.
2. A missing, malformed, truncated, blocked, failed, or in-band-error terminal state
   cannot become `Completed` by default.
3. Every tool result is bound to exactly one call from the same provider, model,
   transport, session, and run.
4. Unsupported request options and unused provider configuration fail before network
   I/O. No configured field is silently ignored.
5. Provider-supplied error text never reaches public errors, session persistence,
   traces, CLI/TUI output, MCP output, or public stream events.
6. Retries preserve request semantics. Retry logic never changes API mode, reasoning,
   tools, messages, or tool results.
7. Active provider state is bounded, opaque, redacted under `Debug`, scoped to one
   active run, and never durable.
8. Complete and streaming paths produce the same normalized terminal contract.
9. Provider call IDs and opaque state remain private transport data. Persisted session
   records retain normalized tool intent, approval, result, and reconciliation evidence.

### Provider-Neutral Domain Model

Replace raw wire-shaped request fields with typed domain values. Exact Rust names may
change during implementation, but the ownership and validation boundaries are fixed:

- `LlmRequest` contains typed messages, tool definitions, generation options, request
  scope, and an optional active-turn continuation.
- `LlmMessage` represents supported system, user, and assistant content without wire
  role coercion. Tool results use a dedicated type rather than a synthetic user string.
- `ToolDefinition` contains a validated name, bounded description, and bounded JSON
  input schema. JSON remains appropriate for schemas and tool argument/result values;
  it must not represent the surrounding message or tool protocol.
- `GenerationOptions.temperature` is optional. `None` means provider default and is
  not an ignored value; `Some` contains a finite typed value in the neutral `0..=2`
  range, with any narrower provider range validated by the adapter. Existing planner,
  agent, and compression constants become provider default because nib exposes no
  user-configured temperature today. A future explicit temperature must be serialized
  or rejected before I/O; Responses may not silently discard it.
- `GenerationOptions.reasoning` distinguishes provider default reasoning, explicitly
  disabled reasoning, and a requested neutral effort. Adapters map only structurally
  supported values and reject the rest.
- `ToolInvocationId` is a nib-generated durable identity used for approval, audit,
  execution, result persistence, and no-reexecution reconciliation. It contains no
  provider value.
- `ProviderCallHandle` is a sealed, in-memory, turn-bound correlation handle. The
  active-turn state privately maps each durable `ToolInvocationId` to a native provider
  call ID or to adapter-owned correlation metadata when the protocol has no call ID.
- `ToolCall` exposes the durable invocation ID, validated name, and bounded arguments;
  raw provider identity remains inside active-turn state.
- `ToolResult` references the durable invocation ID and contains bounded output plus an
  explicit success/error classification.
- `CompletedTurn` contains normalized content, terminal outcome, validated tool calls,
  a safe normalized finish classification, and optional opaque active-turn state.
- `LlmDelta` contains only sanitized model text and tool-call assembly deltas. Agent,
  workload, approval, and UI lifecycle events remain outside the provider stream type.
- `LlmError` is typed and safe to display. It contains provider identity, transport,
  model context after redaction, error class, optional HTTP status, bounded safe
  request ID, retry disposition, and operator guidance. It never contains a raw body.

`CompletedTurn` may represent a completed answer, completed tool request, or a
provider refusal. Incomplete, failed, malformed, and ambiguous responses return
`LlmError`; they are not successful turns with an arbitrary finish string.

### Provider Boundary and Shared Codecs

Introduce one concrete `LlmProvider` implementation for each configured provider:

- `OpenAiProvider`
- `XaiProvider`
- `OpenRouterProvider`
- `MetaProvider`
- `AnthropicProvider`
- `GeminiProvider`
- `MockProvider`

These implementations may compose shared components such as:

- bounded HTTP response readers,
- SSE framing and byte/event budgets,
- request cancellation,
- a parameterized retry executor,
- Chat Completions request/response codecs, and
- Responses request/response codecs.

Provider implementations must not duplicate a wire codec solely to create a different
type name. They must own the behavior that actually differs: endpoint resolution,
accepted configuration, transport selection, option mapping, capability validation,
headers, error envelope decoding, terminal mapping, retry hints, and diagnostics.

Replace duplicated provider-name matches and model/default tables with one registry of
provider descriptors. The registry is the authoritative source for provider identity,
display name, default model, credential environment variable, supported transports,
config parser, and factory constructor. A provider is not advertised as ready merely
because its client can be constructed.

### Structural Capabilities

Each provider exposes immutable structural capabilities derived from its implemented
protocol, not from mutable model names. At minimum they cover:

- supported wire transports,
- complete and streaming support,
- custom function tools,
- structured correlated tool results,
- supported reasoning option forms,
- configurable endpoint shape,
- provider-specific terminal and refusal forms,
- in-band error envelopes,
- retryable HTTP statuses and retry hints, and
- credential-rotation policy.

One common request validator consumes these capabilities before either `complete` or
`stream`. Provider adapters may add stricter wire validation. Both paths must reject
the same unsupported request.

Capabilities do not claim that every model served by the provider supports every
structurally representable combination. Existing T021 behavior remains: an explicitly
configured Chat request with tools and reasoning may be sent when the wire contract can
represent it; a model-level rejection is returned as a safe non-retryable error without
fallback.

### Native Tool-Turn Continuation

The agent loop passes neutral tool results plus opaque active-turn state back to the
same provider implementation. Each adapter maps the turn using its native protocol:

- Responses replays the required ordered output items and appends matching
  `function_call_output` items, retaining T021's encrypted-reasoning handling.
- Chat Completions replays the assistant `tool_calls` message and emits one `tool`
  message with `tool_call_id` for every result.
- Anthropic replays assistant `tool_use` blocks and emits user `tool_result` blocks
  with the matching `tool_use_id` in provider-required order.
- Gemini replays the model function-call content and emits matching
  `functionResponse` content. Required thought signatures or other opaque turn values
  are preserved privately when returned by the protocol. Where Gemini supplies no
  native call ID, the adapter creates an opaque turn-bound correlation handle and
  validates result count, original call order, function name, and retained signature
  metadata before encoding the follow-up.
- Mock models the same neutral call/result lifecycle and declares which optional
  features it supports. It must not silently accept unsupported fields.

The continuation rejects missing, duplicate, foreign, already-consumed, or reordered
results where ordering is provider-significant. Parallel tool calls remain supported.
No adapter may fall back to embedding tool results as ordinary prompt prose during an
active structured tool turn.

Crash and restart behavior remains intentionally non-durable. After active-turn state
is lost, nib reconciles the interrupted run terminally, does not execute a completed
tool twice, and treats any later user-started run as a new provider turn using only
normalized session evidence.

### Terminal-State Normalization

Every provider adapter defines an exhaustive mapping from documented terminal
envelopes to the neutral outcome. The mapping must:

- reject a missing terminal marker rather than inventing `stop`, `STOP`, or `end_turn`,
- reject HTTP-200 response bodies or SSE events that contain provider errors,
- treat token/output truncation as incomplete rather than completed,
- map supported refusals and safety blocks to a non-executable refusal or a typed safe
  error according to the provider contract,
- reject a terminal reason inconsistent with the assembled tool-call set,
- reject tool calls with missing IDs where the native result protocol requires IDs,
- reject incomplete or malformed streamed tool arguments, and
- ignore no terminal error event silently.

Provider-native finish values may be retained privately for debugging classification,
but persisted and public behavior uses bounded normalized reasons. ToolExecutor sees
tool calls only after this validation succeeds.

### Safe Error Contract

All provider HTTP and SSE errors pass through the same safety policy:

- Read error bodies with the existing byte bounds.
- Parse only stable status/code/classification fields needed for control flow.
- Omit provider-supplied messages, metadata, prompt echoes, and arbitrary nested data
  from public errors.
- Redact all configured credentials and encoded sensitive variants from the complete
  diagnostic context, including provider name, model, endpoint path, and request ID.
- Escape control characters and cap the final display string.
- Allow only locally reconstructed, bounded, escaped, and redacted provider ID, selected
  transport, model label, endpoint path, local error enum, numeric HTTP status, retry
  disposition, and operator actions in the public error.
- Allow a provider request ID only from a documented response header after strict ASCII
  syntax and length validation plus redaction. Arbitrary remote error codes are mapped
  to a local enum; unknown values become `ProviderRejected` and are not rendered.
- Keep non-transient semantic 4xx responses non-retryable.
- Deliver the same safe error through completion, stream completion, reconciliation,
  CLI/TUI, MCP, and session failure records.

The T021 tools-plus-reasoning diagnostic remains actionable. Guidance is selected from
the local request tuple, selected transport, and safe error classification; the remote
message is neither trusted for control flow nor repeated verbatim.

### Retry and Credential Policy

Retain one bounded retry executor parameterized by provider policy. T022 uses these
initial policies, which adapters may narrow when authoritative documentation requires
it but may not broaden without fixture-backed evidence:

- One logical provider call has a global maximum of three network attempts regardless
  of credential count. An attempt that wrote a request and received a response counts.
- Connect failures and timeouts before a valid response, plus HTTP `408`, `425`, `429`,
  `500`, `502`, `503`, and `504`, are retryable for OpenAI, xAI/Grok, OpenRouter, Meta,
  Anthropic, and Gemini. Anthropic additionally treats HTTP `529` as retryable.
- Mock performs no transport retry.
- A syntactically valid `Retry-After` on `429`, `503`, or Anthropic `529` is honored but
  capped at 30 seconds. Invalid, negative, past-date, or larger hints use deterministic
  bounded exponential backoff capped at 30 seconds.
- Credential rotation occurs only after HTTP `429`, moves to the next configured
  credential for the next attempt, and never expands the three-attempt global budget.
  Connect failures, timeouts, and other transient statuses retry the same credential.
  Authentication/authorization failures do not rotate or retry.
- Every retry preserves method, endpoint, body, headers other than the selected
  credential and transport-generated request metadata, API mode, reasoning, tools,
  messages, and tool results.
- Partial streamed output, documented in-band errors, invalid requests, unsupported
  options, safety decisions, refusal, and protocol failures are never retried.
- Cancellation and receiver drop interrupt backoff and network reads promptly.

Provider descriptors own the status allowlist and whether a documented response header
can supply a retry hint. The shared helper owns the three-attempt budget, capped delay,
cancellation, and credential index. Error values record whether no retry was attempted,
identical retries were exhausted, or the global budget ended after credential rotation,
without exposing credential identity.

### Configuration and Endpoint Resolution

Resolve the existing TOML schema into a typed provider configuration before client
construction. Backward-compatible valid entries continue to load, but every configured
field must be consumed or rejected.

- Persisted provider IDs remain exactly `openai`, `anthropic`, `google`, `grok`,
  `openrouter`, `meta`, and `mock`; internal `GeminiProvider` and `XaiProvider` names do
  not rewrite user configuration.
- OpenAI-compatible `api` and `reasoning_effort` retain T021 semantics.
- Existing explicit `api = "responses"` selections for OpenAI, Grok, OpenRouter, Meta,
  and custom endpoints remain explicit operator opt-ins and are encoded through the
  Responses codec. Lack of a provider default claim does not silently rewrite an
  explicit selection to Chat; a remote incompatibility returns a safe typed error.
- Native providers reject OpenAI-specific fields until a separate native reasoning
  contract exists.
- Every network provider either consumes `base_url` through provider-specific endpoint
  normalization or rejects it during config validation. Anthropic and Gemini will
  support bounded custom roots/endpoints so local fixtures, explicit gateways, and
  operator-selected proxies do not require private constructors.
- Endpoint validation continues to require absolute HTTP(S), no embedded credentials,
  no query or fragment where the protocol does not explicitly support it, no doubled
  API suffix, and bounded safe diagnostics.
- `nib doctor` reports the provider implementation, selected wire transport, endpoint
  path, structural capabilities, reasoning mode, and any model-dependent compatibility
  warning without a paid request.
- Provider defaults must be backed by current authoritative documentation and a
  credential-free wire fixture. If a listed provider's default endpoint or protocol
  cannot be verified, nib must require an explicit endpoint or stop advertising that
  provider as ready instead of routing it through a generic alias.
- Dated documentation links, observed fixture shapes, and any support limitation are
  recorded in a T022 `Implementation Evidence` section and the owning provider fixture
  comments before the spec moves to done.

### Provider-Specific Requirements

#### OpenAI

- Preserve T021's explicit Chat Completions and Responses modes.
- Preserve Responses as the default for newly authenticated canonical OpenAI entries.
- Preserve bounded private Responses continuation and `store: false` behavior.
- Apply the shared terminal, error, retry, and conformance contracts to both modes.

#### xAI/Grok

- Use a distinct provider implementation around any shared Chat or Responses codec.
- Keep Chat as the default unless fixture-backed authoritative documentation supports a
  different default. Preserve an operator's explicit T021 Responses opt-in without
  claiming universal remote model support.
- Own xAI endpoint, headers, errors, retry classification, and diagnostics.

#### OpenRouter

- Decode HTTP-200 in-band Chat and streaming errors before terminal authorization.
- Treat `finish_reason = "error"` and a top-level or choice-level error as failure.
- Honor bounded documented retry hints and keep routed-provider metadata private.
- Enable Responses only when its complete tool loop passes dedicated fixtures.

#### Meta

- Use a distinct adapter with an authoritative endpoint and protocol fixture.
- Do not treat an OpenAI-compatible claim as proof that OpenAI-specific errors,
  reasoning fields, Responses continuation, or retry behavior are identical.
- Fail default configuration/doctor readiness if the default endpoint cannot be
  verified. Preserve explicit T021 transport selection without silently changing it.

#### Anthropic

- Preserve `tool_use.id` and send matching `tool_result.tool_use_id` blocks.
- Parse SSE error events and documented stop reasons exhaustively.
- Classify overload and retry behavior without exposing provider messages.
- Reject missing stop reasons and inconsistent `tool_use` terminal states.

#### Gemini

- Send structured `functionResponse` results instead of prompt prose.
- Preserve required opaque thought signatures privately and bind them to the active
  turn when the protocol returns them.
- Normalize safety blocks, refusals, truncation, and finish reasons exhaustively.
- Reject missing finish reasons and malformed function calls.

#### Mock

- Implement the same typed contract and shared conformance assertions without network
  I/O.
- Declare deterministic capabilities and reject unsupported reasoning or continuation.
- Keep scenario scripting separate from production request validation.

### Caller and Persistence Boundaries

Update every direct LLM caller to build the typed request and handle typed outcomes:

- planner,
- main agent loop,
- context compression,
- delegated and durable runs,
- CLI/chat/TUI entry points, and
- MCP/gateway paths that observe model output or failures.

The authoritative session/workload model remains provider-neutral. It persists
normalized assistant content, durable nib tool invocation ID, tool intent, approval,
execution result, failure class, and reconciliation. Private provider call handles and
opaque state do not enter public observers or session JSON. During one active structured
turn, results return only through the opaque provider continuation. On a later new run,
historical persisted tool evidence may be rendered as bounded normalized context; it is
not represented as a resumed native tool result. A crash after persisted tool completion
continues to reconcile without re-execution under the existing workload rules.

## Implementation Plan

1. Add failing regression fixtures for native raw error exposure, OpenRouter in-band
   errors, missing/unsafe terminal states, ignored config fields, and Mock option drift.
2. Introduce typed neutral request, message, tool, result, completed-turn, stream-delta,
   capability, and safe-error types; remove positional temperature arguments and make
   current provider-independent callers request provider-default sampling explicitly.
3. Create the provider descriptor registry and distinct provider implementations,
   initially delegating to the existing codecs without changing valid wire payloads.
4. Generalize active-turn state and implement exact structured two-request tool loops
   for Chat Completions, Anthropic, Gemini, Responses, and Mock.
5. Make terminal mapping and provider error decoding exhaustive and fail closed for
   both completion and streaming paths.
6. Parameterize retry/credential behavior and make all configured endpoints and fields
   either consumed or rejected.
7. Apply the shared conformance suite to every registered provider and add
   credential-free full runtime fixtures for each wire dialect.
8. Update architecture, Rust conventions, user configuration guidance, doctor output,
   provider support claims, and related specs to match the delivered contract.
9. Perform separate spec-compliance and technical/security reviews, reconcile all
   findings, run canonical gates, and record exact evidence before moving T022 to done.

## Rollout Plan

- Phase 1 lands immediate fail-closed error and terminal regressions without changing
  valid provider payloads.
- Phase 2 introduces the typed core and provider registry behind the existing factory.
- Phase 3 adds each distinct provider implementation together with its native structured
  continuation and requires the full provider conformance suite to pass while the old
  factory path remains active.
- Phase 4 activates providers one at a time in the factory only after their complete,
  stream, error, terminal, retry, and two-request tool-loop conformance gates pass.
- Phase 5 removes obsolete raw request/response paths and duplicated provider tables.

No release may advertise mixed contract semantics: every configured supported provider
must either use the new contract or fail clearly as unavailable. Existing valid TOML
continues to load. No persisted provider-state migration is required because active
turn state is deliberately in-memory only. T010 owns artifact publication; T021 keeps
ownership of the original incident's release/default migration evidence.

## Alternatives Considered

### Keep One OpenAI-Compatible Provider Alias

Rejected. Sharing a codec is appropriate, but one alias cannot safely normalize
provider-specific in-band errors, endpoints, retry hints, or support claims.

### Duplicate One Full Client Per Provider

Rejected. Copying HTTP, SSE, Chat, and Responses logic would multiply parser and safety
drift. Distinct provider implementations should compose shared tested codecs.

### Keep Raw JSON as the Neutral Contract

Rejected. Raw JSON makes invalid roles and tools adapter-dependent and permits silent
loss. JSON remains only at the bounded schema, argument, and result leaves.

### Add a Model Capability Catalog

Rejected. Aliases, gateways, and provider deployments change independently of nib.
The contract models stable protocol capabilities and leaves model-dependent behavior to
explicit configuration plus safe provider rejection.

### Automatically Retry Through Another API or Disable Reasoning

Rejected. Semantic fallback obscures the effective request, may duplicate paid work,
and violates T021's explicit operator-controlled behavior.

### Persist Provider Continuation

Rejected. Provider state may contain opaque reasoning or short-lived identifiers and
would complicate local privacy and restart correctness. Existing terminal crash
reconciliation is safer and auditable.

## Risks and Tradeoffs

- **Large behavioral surface:** Every LLM caller and provider is affected. Mitigation:
  introduce typed types mechanically, port one adapter at a time, and require shared
  conformance before factory activation.
- **Provider documentation drift:** Wire formats and defaults can change. Mitigation:
  fixture each claimed contract, date authoritative references, avoid model catalogs,
  and fail readiness rather than guess.
- **Overly strict terminal parsing:** A provider may add a valid terminal reason.
  Mitigation: treat unknown values as safe protocol errors with clear diagnostics and
  update the isolated provider adapter after fixture evidence.
- **Reduced provider error detail:** Omitting remote messages may make troubleshooting
  harder. Mitigation: retain safe provider/status/request-ID context and specific local
  guidance without persisting raw bodies.
- **Active-turn memory growth:** Native continuation can contain opaque provider data.
  Mitigation: preserve T021 item/byte limits, validate every append, and drop state on
  cancellation or reconciliation.
- **Retry behavior changes:** Respecting provider hints can alter latency or credential
  use. Mitigation: bound delays and attempts, expose retry disposition, and test policy
  deterministically.
- **Sampling behavior changes:** Removing hard-coded caller temperatures lets providers
  use their defaults until nib exposes an explicit sampling setting. Mitigation: record
  the migration, fixture request omission, and reject rather than drop any future
  explicit temperature.
- **Backward compatibility:** Rejecting previously ignored fields can break invalid
  configurations that appeared to work. Mitigation: diagnose the exact unused field and
  provide an explicit migration action; do not continue silently.

## Acceptance Criteria

### Neutral Contract

- [ ] Every production and Mock provider implements one typed `LlmProvider` contract;
  callers do not branch on provider names or wire formats.
- [ ] Core messages, tools, tool calls, tool results, terminal outcomes, stream deltas,
  capabilities, and errors are typed. Raw JSON is limited to validated schemas,
  arguments, results, and private adapter payloads.
- [x] `GenerationOptions` represents provider-default versus explicit finite
  temperature and reasoning. Every explicit value is serialized or rejected before
  I/O, and no current caller relies on a silently discarded positional temperature.
- [ ] One provider registry owns supported names, display metadata, defaults,
  credentials, config resolution, capabilities, and constructors.
- [ ] Complete and streaming entry points share request validation and return equivalent
  normalized outcomes.

### Tool Continuation

- [ ] Credential-free two-request fixtures prove tool call, exact correlated result,
  and final answer for Responses, Chat Completions, Anthropic, Gemini, and Mock.
- [ ] Parallel tool fixtures prove every result is matched once and missing, duplicate,
  foreign, replayed, or cross-session call IDs fail before network I/O.
- [ ] Every tool call has a persisted nib `ToolInvocationId` and a separate in-memory
  provider correlation handle; raw native IDs never become durable workload identity.
- [ ] Anthropic emits `tool_result` with the exact `tool_use_id`; Gemini emits structured
  `functionResponse`; Chat emits `tool` messages with `tool_call_id`; Responses emits
  `function_call_output` with `call_id`.
- [ ] Gemini fixtures without native call IDs use adapter-generated turn-bound handles
  and reject mismatched count, order, name, or required signature metadata.
- [ ] Provider call IDs, opaque continuation, reasoning items, and thought signatures
  never appear in public streams, debug output, logs, persisted sessions, CLI/TUI, or
  MCP responses.
- [ ] Kill/restart after durable tool completion reconciles terminally and proves the
  tool is not executed twice when opaque turn state is lost.

### Errors and Terminal Authority

- [ ] Complete and stream fixtures for every network provider prove HTTP 4xx/5xx and
  documented in-band errors where applicable return bounded typed safe errors without
  provider body text.
- [ ] Error fixtures include raw, URL-encoded, JSON-escaped, and control-character
  remote-only sentinel echoes of prompts, active and inactive credentials, and values
  resembling provider/model/endpoint labels; none reaches output or persistence.
  Separately reconstructed local provider/transport/status/model/path context remains
  present only after bounding, escaping, and redaction.
- [ ] OpenRouter HTTP-200 completion and SSE errors with `finish_reason = "error"` fail
  and cannot become a completed workload outcome.
- [ ] Missing terminal markers, truncation, content/safety blocks, refusals, inconsistent
  tool terminal states, malformed tool arguments, and premature EOF are exhaustively
  tested for every applicable provider and both request modes.
- [ ] Only a validated completed private turn can reach ToolExecutor. Partial/public
  stream events and refused, failed, incomplete, or ambiguous turns cannot authorize a
  tool.

### Configuration, Capabilities, and Retry

- [ ] Legacy TOML continues to parse and retains effective provider/API/reasoning
  defaults; every configured field is then consumed or rejected with an actionable
  error instead of remaining a silent no-op.
- [ ] Anthropic and Gemini custom `base_url` values reach their adapters through public
  configuration and pass the same endpoint-security checks as compatible providers.
- [ ] Unsupported provider/request option combinations, malformed messages/tools,
  invalid schemas, and non-finite generation values fail identically in complete and
  stream before network I/O.
- [ ] New canonical OpenAI still defaults to Responses; legacy/custom OpenAI, Grok,
  OpenRouter, and Meta retain only their explicitly documented T021 transport defaults.
- [ ] Persisted provider IDs remain stable, and existing explicit Responses selections
  are neither rewritten to Chat nor advertised as a provider default without evidence.
- [ ] The tools-plus-reasoning matrix proves canonical OpenAI avoids the reported Chat
  tuple, native providers never receive OpenAI reasoning fields, Mock does not ignore
  unsupported reasoning, and an explicit model-dependent Chat rejection is safe,
  actionable, non-retried, and never semantically downgraded.
- [ ] Retry fixtures cover each provider's retryable statuses, bounded `Retry-After`,
  the global three-attempt limit, cancellation, receiver drop, 429-only credential
  rotation, Anthropic 529, and proof that retried request semantics remain identical.
- [ ] `nib doctor` reports resolved provider implementation, transport, endpoint path,
  structural capabilities, reasoning mode, and compatibility warnings without a paid
  request or sensitive value.

### Provider Conformance and Runtime

- [ ] The same conformance harness runs against OpenAI, xAI/Grok, OpenRouter, Meta,
  Anthropic, Gemini, and Mock for complete/stream parity, validation, errors, terminal
  states, limits, cancellation, and tool correlation.
- [ ] Each advertised default endpoint and transport has dated authoritative evidence
  and a credential-free request/response fixture; unverifiable defaults fail readiness
  instead of being advertised.
- [ ] Planner and full agent-loop fixtures complete a structured plan and one approved
  tool round trip through every implemented wire dialect while preserving truthful
  session and workload reconciliation.
- [ ] CLI, TUI, MCP, delegated runs, durable runs, and context compression handle typed
  provider failures without leaking private provider state or leaving work running.
- [ ] Architecture, Rust conventions, user guide, provider inventory, T021 relationship,
  and config/doctor documentation describe the delivered behavior without overstating
  model-specific compatibility.
- [ ] T021 exact-revision evidence either predates all T022 runtime changes as the
  recorded baseline or is rerun on the final T022 revision according to the sequencing
  rule above; stale binary evidence cannot close either spec.
- [ ] Independent spec-compliance and technical/security reviews find no unresolved
  blocker, and all validation gates pass on the exact implementation revision.

## Affected Areas

- `src/llm/mod.rs`, `src/llm/types.rs`, and new or reorganized contract/error/stream
  modules
- `src/llm/factory.rs` and the provider registry
- `src/llm/openai.rs`, `src/llm/responses.rs`, `src/llm/anthropic.rs`,
  `src/llm/gemini.rs`, `src/llm/mock.rs`, and provider-specific wrappers/codecs
- `src/config/mod.rs`, `src/auth.rs`, `src/config_cmd.rs`, and `src/doctor.rs`
- `src/agent/planner.rs`, `src/agent/loop.rs`, and run reconciliation
- `src/context/`, delegated/durable execution, CLI/chat/run, TUI, gateway, and MCP
  observer paths
- session/audit tests that prove private provider state is not persisted
- provider unit fixtures and `tests/test_runtime_e2e.rs`
- `README.md`, `docs/user/guide.md`, `docs/tech/architecture.md`,
  `docs/tech/backend_rust.md`, `docs/projects/nib/inventory.md`, and related specs

## Validation Gates

- Failing-first credential-free regression fixtures for every audit finding
- Shared provider conformance suite for complete and streaming paths
- Provider-native two-request tool-loop fixtures for every wire dialect
- Typed config migration, unused-field rejection, endpoint, doctor, and redaction tests
- Terminal/refusal/truncation/error and retry/cancellation matrices
- Planner and full runtime E2E with authoritative reconciliation and no duplicate tool
  execution after interrupted continuation
- Public observer and persistence isolation tests for call IDs and opaque state
- `task fix`
- `task check`
- `task test`
- `task docs:check`
- `task check:all-targets`
- `task coverage`
- `task build`
- `git diff --check`
- Separate spec-compliance review followed by technical/security review
- Exact-revision Linux, macOS, and Windows CI evidence; paid live-provider smoke remains
  optional and must never be the sole contract evidence

## Open Questions

No blocking design questions remain for this scope. Native Anthropic/Gemini reasoning,
multimodal messages, hosted provider tools, and durable provider continuation require
separate specs rather than expansion of T022.

## Hosted Fixture Reconciliation (2026-08-02)

The first hosted matrix run after the provider-neutral implementation found eight
fixtures that constructed tool-bearing responses without the request scope required by
the sealed continuation contract. OpenAI, Anthropic, and Gemini completion/stream
fixtures now supply a deterministic session/run scope. The Anthropic and Gemini
negative fixtures continue to use a deliberately foreign OpenAI continuation, but now
assert the binding error because both adapters support their own native continuation.

This is a fixture and lint reconciliation, not a weakening of continuation validation:
provider/model/API/session/run mismatches still fail before I/O. The exact local
`task check` gate passes; exact-revision hosted matrix evidence and the broader
unchecked conformance criteria remain open.

## Implementation Progress (2026-08-21)

Shipped on this host, still insufficient to move the spec to done:

- One agent-facing `LlmClient` port, `LlmError`/`LlmErrorClass`/`RetryDisposition`,
  opaque `ProviderContinuation`, and a registry/factory for OpenAI, xAI/Grok,
  OpenRouter, Meta, Anthropic, Gemini, and Mock.
- Per-adapter complete/stream fixtures for HTTP errors, redaction, continuation
  binding, and OpenAI-compatible complete/stream class equality.
- Typed `LlmRequest` messages (`LlmMessage` system/user/assistant) and
  `ToolDefinition` (validated name, description, JSON schema). JSON remains only for
  schemas, tool arguments, and private adapter payloads on the request boundary.
- `GenerationOptions` with optional finite `0..=2` temperature and
  provider-default / disabled / explicit reasoning. Planner, agent, and compression
  callers use provider default. Chat/Anthropic/Gemini omit default temperature and
  serialize explicit values; Responses and Mock reject explicit temperature before
  I/O; Anthropic, Gemini, and Mock reject non-default reasoning before I/O.
- Shared `src/llm/conformance.rs` fixtures for temperature/reasoning validation and
  OpenAI-compatible complete/stream HTTP 401 class equality.

Remaining before Done:

- Completing the rest of the typed domain (`LlmProvider` naming, structural
  capability objects, dedicated tool-result request types beyond continuation).
- Expanding the shared harness to every terminal state, cancellation, and native
  tool-correlation scenario for Anthropic, Gemini, and Mock, not only options and
  OpenAI-compatible 401s.
- T021 exact-revision release-binary sequencing, coverage/all-targets, and independent
  spec-compliance plus technical/security review.

## External References

- [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling)
- [OpenAI Responses migration](https://developers.openai.com/api/docs/guides/migrate-to-responses)
- [Anthropic tool-use loop](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)
- [Gemini function calling](https://ai.google.dev/gemini-api/docs/function-calling)
- [OpenRouter streaming error handling](https://openrouter.ai/docs/api/reference/streaming)
- [OpenRouter errors and retry hints](https://openrouter.ai/docs/api/reference/errors-and-debugging)
