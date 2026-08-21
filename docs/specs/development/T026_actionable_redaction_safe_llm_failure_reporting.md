# T026: Actionable, Redaction-Safe LLM Failure Reporting

**Status:** Development

**Related:**
[T007: Configuration Schema Alignment and Doctor Validation](../development/T007_configuration_schema_alignment_and_nib_doctor_validation.md),
[T022: Provider-Neutral LLM Contract and Adapter Conformance](../development/T022_provider_neutral_llm_contract_and_adapter_conformance.md),
[T025: Interactive Chat and TUI Capability Parity](../done/T025_interactive_chat_tui_capability_parity.md), and
[FT-011: LLM Streaming and TUI](../done/ft_011_llm_streaming_and_tui.md)

## Summary

Make LLM failures diagnosable without exposing provider-controlled text, credentials,
prompt echoes, or opaque provider state. Preserve T022's typed safe error from the
provider adapter through the agent result, reconciliation record, CLI, chat, TUI, MCP,
and gateway boundaries. Each user-facing surface must render a concise local error
class, safe request context, retry disposition, and deterministic next action using its
native presentation system.

This task also removes literal Rich-style markup from line-mode output. Chat and run
must emit terminal styling only through a renderer that can produce ANSI for a capable
interactive terminal and plain text for non-TTY, redirected, `NO_COLOR`, test, and
machine-readable output.

## Observed Incident

On 2026-08-15, an interactive chat turn displayed an error equivalent to:

```text
[dim]Thinking...[/dim]
[red]Error during step: agent run failed for session <id>: ...
provider returned a structured error; provider-supplied detail omitted
```

Omitting the provider's free-form message is intentional and security-preserving. The
failure is nevertheless defective because the output does not retain a stable local
classification or useful recovery action, repeats internal run/reconciliation wording,
and exposes presentation tags that the Rust console does not interpret.

## Root-Cause Analysis

| Boundary | Current behavior | Consequence |
| --- | --- | --- |
| OpenAI-compatible Chat and Responses adapters | A parsed remote error envelope is collapsed to a generic `String`; safe HTTP/provider/model context is concatenated around it. | The untrusted message is correctly omitted, but authentication, quota, model, unsupported-request, and transient classes cannot be handled reliably downstream. |
| `LlmClient` and `LlmStream` | Completion and stream failures use `Result<_, String>`. | Type, retry state, and safe structured context are irreversibly flattened. |
| Agent loop | The string becomes an `llm_stream_failed: ...` outcome and is persisted during reconciliation. | Machine state and display prose are coupled; callers must parse or repeat an internal string. |
| Chat/run wrappers | The failure outcome is wrapped again as `agent run failed for session ...`. | The user sees nested implementation wording rather than one actionable incident report. |
| Chat presentation | `src/chat.rs` prints `[dim]`, `[red]`, `[yellow]`, `[green]`, and `[bold]` tokens directly, mixed with raw ANSI escapes. | Styling tokens appear literally and redirected output is inconsistent. |
| Conversation persistence | Reconciliation failures can be represented as assistant messages, and chat has a fallback that appends `[error] ...` as assistant content. | A local operational failure can masquerade as model-authored conversation content on a later turn. |

The reported line is therefore evidence of a real provider rejection plus a local
diagnostic/presentation defect. It is not sufficient evidence to infer the provider's
underlying cause because nib intentionally discarded the remote message and does not
yet preserve a typed safe classification.

## Decision and Ownership

- T022 continues to own the canonical provider-neutral `LlmError`, provider-specific
  decoding, terminal authority, retry policy, and safe-field allowlist. T026 must
  consume that type; it must not introduce a competing provider error hierarchy.
- T026 owns propagation of a safe LLM failure through the agent summary and observer
  boundaries, structured reconciliation evidence, deterministic operator guidance,
  and presentation behavior.
- T025 owns general chat/TUI command and capability parity. Any shared interactive
  renderer introduced by T025 is reused by T026; whichever change lands first must
  leave one presentation abstraction rather than parallel console helpers.
- T026 implementation may land with T022 or after the relevant T022 error slice. It
  must not ship a string parser that guesses error meaning from existing display text.

## Goals

- Show users what safely happened: provider, selected transport, redacted model,
  failure class, numeric HTTP status when available, retry disposition, and one local
  recovery action.
- Keep the provider's free-form error message and arbitrary metadata private and
  non-durable.
- Keep machine outcome, structured diagnostic evidence, and rendered prose separate.
- Preserve the same safe classification across complete, streaming, agent, CLI/TUI,
  MCP, gateway, delegated, durable, and compression paths.
- Ensure a provider failure never becomes assistant-authored conversational content.
- Produce readable native terminal output with no literal markup and no accidental
  ANSI/control sequences in plain or protocol output.

## Non-Goals

- Displaying raw provider error bodies or free-form provider messages, even behind a
  verbose or debug flag.
- Automatically changing a model, provider, API mode, reasoning setting, or tool
  request after rejection.
- Retrying semantic failures or expanding T022's retry budget.
- Adding telemetry, uploading session diagnostics, or introducing a hosted support
  service.
- Replacing T025's broader interactive parity work.
- Guaranteeing a precise cause when the response exposes only an unknown or ambiguous
  failure. Such responses remain `ProviderRejected` with conservative guidance.

## Functional Contract

### Safe Failure Model

The T022 `LlmError` is carried intact to an agent-facing failure record. Exact Rust
names may change with T022, but the downstream record must retain only allowlisted safe
fields:

- local error class;
- registered provider ID and selected transport;
- model label after full sensitive-value redaction and control escaping;
- request phase, such as connect, HTTP response, stream, terminal validation, or
  continuation;
- numeric HTTP status when a valid response supplied one;
- retry disposition: not retryable, not attempted, exhausted, or cancelled;
- bounded `Retry-After` duration only when accepted by T022's retry policy;
- bounded provider request ID only from a documented header after strict validation;
- stable local incident code and deterministic operator action.

It must never contain a raw response body, provider message, arbitrary remote metadata,
prompt fragment, credential, provider continuation, native tool-call ID, complete URL,
or unvalidated header value. Provider, model, endpoint-derived, and request-ID fields
are not trusted merely because nib supplied or selected them; the complete record is
redacted, escaped, and bounded before it crosses the adapter boundary.

### Local Classification and Guidance

Guidance is selected from local request state, HTTP status, retry result, and an exact
allowlist of documented structural provider codes. Unknown remote codes are not shown
and map conservatively.

| Local class | Minimum safe evidence | Default action |
| --- | --- | --- |
| `Configuration` | provider/config field, no network attempt | Run `nib config validate` or complete `nib auth`. |
| `Authentication` | HTTP 401/403 or exact documented authentication class | Refresh that provider's credential with `nib auth`; do not print or identify a credential value. |
| `RateLimited` | HTTP 429 and retry disposition | Retry after the bounded hint when present; otherwise retry later. |
| `QuotaOrBilling` | Exact documented, allowlisted structural class | Check the provider account's quota/billing controls; do not infer this from free-form text. |
| `ModelUnavailable` | Exact documented class or unambiguous endpoint/status contract | Verify the configured model with `/model` or provider configuration. |
| `UnsupportedRequest` | Local validation or exact documented 400/422 class | Run `nib doctor` and verify the selected transport/reasoning/tool combination. |
| `ProviderUnavailable` | Exhausted retryable status | Retry later; report that bounded retries were exhausted. |
| `Transport` | Connect failure or timeout class | Check network reachability and the configured endpoint, then retry. |
| `Protocol` | Invalid envelope, missing terminal marker, premature EOF, or unsafe in-band error | Run `nib doctor`; retry only after configuration/provider compatibility is verified. |
| `ProviderRejected` | A valid but otherwise unclassified rejection | Report the status if available and direct the user to `nib doctor`; do not guess. |

Safety refusal and normal model refusal retain T022's non-executable terminal semantics
and are not relabeled as transport failures.

### Agent and Persistence Boundary

- `AgentRunSummary.outcome` remains a stable machine outcome such as
  `llm_stream_failed`; human prose and provider context do not live in that field.
- A failed summary exposes an optional structured safe failure record. Successful
  summaries do not carry stale failure data.
- Reconciliation persists the stable outcome plus the bounded safe fields required for
  audit. It does not persist the provider body or rendered sentence.
- Provider failures are recorded as local lifecycle/reconciliation evidence, never as
  role `assistant` content. The context builder must not send a previous local failure
  to a later provider as if the model authored it.
- Existing session files with string-only outcomes remain readable. No migration may
  invent a classification from old prose; absent structure is reported as legacy
  `ProviderRejected` evidence when displayed.
- Cancellation, plan blocking, lease release, and tool non-execution guarantees remain
  unchanged. A presentation failure cannot turn a failed run into success.

### Presentation Contract

All observers consume the structured safe failure through a presentation-neutral
mapper. A typical console report is:

```text
LLM request failed [LLM-AUTH]
Provider: openai (responses), model: gpt-5
HTTP: 401; retry: not attempted
Action: Refresh the OpenAI credential with `nib auth`, then retry.
Session: <id>
```

- Interactive chat displays the report once and remains usable for another command or
  turn when reconciliation completed safely.
- `nib run` emits the concise report to standard error and exits nonzero.
- TUI uses Ratatui spans/widgets and keeps the full safe report inspectable within its
  existing bounded event/detail model.
- MCP and gateway return structured protocol-appropriate failure data where their
  contracts allow it, otherwise one bounded plain-text rendering. They never receive
  ANSI, Rich-style tags, or provider-private fields.
- TTY output may use ANSI through one renderer. Non-TTY, redirected, `NO_COLOR`, test,
  log, persisted, MCP, and gateway output is plain text.
- No public output contains literal presentation tokens such as `[red]` or `[dim]`.
  User/model/tool content is never interpreted as markup.
- The primary line starts with the stable incident code and cause, not nested internal
  wrappers. The session ID is separate context and is not repeated inside the cause.

## Scope

- T022 error propagation at the LLM/agent boundary.
- Agent summary and reconciliation representation for provider failures.
- CLI `run`, interactive `chat`, TUI, MCP, gateway, delegated/durable, planner, and
  compression observers that can surface an LLM failure.
- A shared presentation-neutral failure mapper and terminal capability policy.
- Removal of Rich-style console tokens and mixed ad hoc styling from chat/run paths
  touched by this incident.
- User documentation for safe diagnostics and recovery actions.
- Deterministic provider fixtures and end-to-end output/persistence tests.

## Affected Areas

- `src/llm/mod.rs`, `src/llm/types.rs`, `src/llm/openai.rs`,
  `src/llm/responses.rs`, `src/llm/anthropic.rs`, and `src/llm/gemini.rs`
- provider wrappers/codecs and Mock introduced or reorganized by T022
- `src/agent/loop.rs`, `src/agent/planner.rs`, and context compression callers
- `src/chat.rs`, `src/run.rs`, `src/tui/`, and a shared presentation module
- `src/integrations/gateway.rs` and MCP observer paths
- delegated and durable run result adapters
- `src/session/` serialization and context construction
- `src/doctor.rs` and `docs/user/guide.md`
- provider fixtures, CLI integration tests, TUI `TestBackend` tests, and session
  redaction tests

## Implementation Plan

1. Add failing local HTTP/SSE fixtures that reproduce a structured provider rejection
   containing prompt, credential, encoded-secret, control-character, and long-value
   sentinels. Add an interactive child-process regression proving the observed literal
   markup and nested error output.
2. Land or consume T022's typed `LlmError` and exact provider-specific structural
   classification. Remove `String` flattening from completion and stream error paths;
   do not parse the legacy display sentence.
3. Separate the agent's stable outcome from its optional safe failure record. Persist
   the structured reconciliation evidence additively and stop creating synthetic
   assistant failure messages.
4. Introduce one pure failure-to-presentation mapper plus console capability handling.
   Reuse T025 presentation primitives when available; render Ratatui styling only in
   TUI and terminal styling only at the final console boundary.
5. Update run, chat, TUI, MCP, gateway, planner, compression, delegated, and durable
   observers to preserve the safe class and render the correct action.
6. Add compatibility tests for legacy sessions and update user/technical documentation.
7. Run separate spec-compliance and security/quality reviews, reconcile findings, and
   record exact validation evidence before lifecycle completion.

## Acceptance Criteria

- [ ] Reproducing the reported provider rejection produces one actionable report with
      a stable local incident code, provider, transport, redacted model, HTTP status
      when known, retry disposition, action, and separate session ID.
- [ ] The report contains neither `provider-supplied detail omitted` as its primary
      diagnosis nor nested `agent run failed ... llm_stream_failed` prose.
- [ ] T022's typed safe error reaches every applicable observer without string parsing
      or a competing error type.
- [ ] Complete and streaming paths classify the same fixture identically for OpenAI,
      xAI/Grok, OpenRouter, Meta, Anthropic, Gemini, and Mock.
- [ ] Authentication, rate limit, quota/billing, model unavailable, unsupported
      request, provider unavailable, transport, protocol, cancellation, and unknown
      rejection fixtures select only their documented deterministic action.
- [ ] Unknown or malformed remote codes never become a more specific local class.
- [ ] Raw, URL-encoded, JSON-escaped, and control-character forms of active/inactive
      credentials, prompts, endpoints, model labels, provider messages, and arbitrary
      metadata do not reach errors, debug output, streams, sessions, logs, CLI/TUI,
      MCP, or gateway output.
- [ ] Request IDs appear only from provider-specific documented headers after ASCII,
      syntax, length, redaction, and output-bound checks.
- [ ] HTTP bodies remain bounded and are discarded after safe classification; no debug
      or verbose mode exposes them.
- [ ] Agent outcomes are stable machine values, structured failure evidence is additive
      and bounded, and legacy string-only sessions remain readable without inferred
      detail.
- [ ] A provider failure cannot authorize a tool, alter plan blocking semantics, leave
      a continuation reusable, or be reported as success.
- [ ] Provider failures are never persisted as assistant-authored messages or supplied
      to a later model turn under the assistant role.
- [ ] Chat remains usable after a safely reconciled provider failure; `nib run` returns
      nonzero; TUI, MCP, gateway, delegated, durable, planner, and compression paths
      preserve the same safe classification.
- [ ] Interactive TTY output uses only the selected native renderer. Plain, redirected,
      `NO_COLOR`, test, persisted, MCP, and gateway output contains no ANSI escapes,
      control characters, `[red]`/`[dim]`-style tags, or interpreted user content.
- [ ] Output is bounded under long provider/model/request-ID/session values and remains
      readable at narrow terminal widths.
- [ ] `nib doctor` exposes the resolved local provider/transport/capability context
      needed by every recommended action without making a paid request.
- [ ] User and technical documentation explain the safety boundary, incident codes,
      recovery actions, and where to find the session audit record.
- [ ] `task docs:check`, `task check`, `task test`, `task check:all-targets`,
      `task coverage`, `task build`, and `git diff --check` pass on the exact
      implementation revision.
- [ ] Independent spec-compliance and security/quality reviews have no unresolved
      blocking findings.

## Validation Gates

- Failing-first local HTTP and SSE provider-error fixtures for every registered
  provider and both complete/stream paths
- Shared error-class/action table tests using Mock with no network or credentials
- Agent-loop tests for stable outcome, typed failure propagation, plan blocking,
  continuation abandonment, and no tool authorization
- Session round-trip, legacy compatibility, failure-event isolation, and full
  sensitive-value corpus scans
- CLI child-process tests for exit status, stdout/stderr ownership, one-time rendering,
  `NO_COLOR`, redirected output, narrow width, and literal-markup absence
- Ratatui `TestBackend` failure/detail rendering tests
- MCP, gateway, delegated, durable, planner, and compression observer tests
- Manual raw-terminal smoke for recoverable chat failure followed by a successful turn
- `task docs:check`
- `task check`
- `task test`
- `task check:all-targets`
- `task coverage`
- `task build`
- `git diff --check`

## Risks and Mitigations

- **Duplicating T022:** A second error hierarchy would create drift. T026 is gated on
  T022's canonical type and owns only propagation, persistence, guidance, and display.
- **Overclassification:** Provider formats change and free-form text is untrusted. Only
  exact documented structural fields may select a specific class; unknowns remain
  conservative.
- **Redaction gaps in safe-looking fields:** Model, endpoint, provider, and request-ID
  values can still contain secrets or controls. Redact and bound the complete typed
  record before storage or display, then test encoded variants.
- **Breaking session compatibility:** Changing outcome representation can invalidate
  existing JSON. Use additive optional fields and keep legacy reads deterministic.
- **Renderer injection:** Treat user, model, provider, and tool strings as text spans,
  never as formatting syntax.
- **Cross-spec merge conflicts:** T025 may refactor the same chat/TUI boundary. Reuse
  its shared presentation layer and stage changes so only one owner edits each surface.

## Rollout Plan

1. Merge the failure fixtures and T022 typed error slice without changing current safe
   omission of provider-controlled messages.
2. Activate structured agent propagation and persistence while retaining legacy
   session reads.
3. Switch one-shot CLI/chat and TUI presentation to the new report, then activate MCP,
   gateway, delegated, durable, planner, and compression observers.
4. Run the complete credential-free matrix and canonical gates. Use optional
   credential-gated T023 canaries only as supplemental evidence.
5. Keep rollback compatible with previously persisted string-only outcomes; do not
   roll back by re-enabling raw provider messages.

## Development Validation Snapshot (2026-08-17)

- The focused typed-error tests passed 5/5 and the CLI failure regression passed 1/1.
- `task check` passed formatting, Clippy with warnings denied, compilation, 744 library
  tests, 51 binary tests, every integration group, and doc tests. The explicitly gated
  paid live-provider test remained ignored.
- `task check:all-targets`, `task docs:check` (5/5), `task build`, and
  `git diff --check` passed on the combined T023/T025/T026/T027 tree.
- The three earlier Clippy findings were resolved structurally by clarifying pattern
  naming, grouping LLM failure request metadata, and boxing streamed error results.
- This spec remains in Development: independent `task test`, coverage, raw-terminal
  failure recovery smoke, and final compliance/security review remain completion gates.

## Implementation Progress (2026-08-21)

Typed `LlmError` now reaches agent summaries, chat, `nib run`, TUI activity, gateway,
durable workload, planner, and compression observers. Incident codes, `user_report`,
CLI child-process redaction, and the user-guide recovery table remain in place. This
change set does not close T026: coverage, all-targets, raw-terminal recovery smoke,
MCP observer proof, and independent reviews stay open.

## Open Questions

- Should a strictly validated provider request ID be shown by default or only in an
  expanded/detail view? It remains safe and persisted either way only if T022's header
  allowlist and validation accept it.
- Should the stable incident-code registry live beside T022's `LlmError` or in the
  presentation module? The code-to-class mapping must have one owner regardless of
  module placement.
