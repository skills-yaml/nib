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
- Replacing T022's `provider_continuation_interrupted` structural recovery boundary.
  That bounded local assistant-role tombstone contains no `LlmError`, provider body,
  incident report, action text, or remote continuation. It exists only to close an
  interrupted assistant/tool sequence before a later request and remains governed by
  T022's role-validity and crash-recovery contract.

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
- Typed provider failures, rendered incident reports, and provider rejection prose are
  recorded as local lifecycle/reconciliation evidence, never as role `assistant`
  content. The context builder must not send a previous local failure to a later
  provider as if the model authored it. T022's bounded
  `provider_continuation_interrupted` structural tombstone is not a provider failure
  or rendered report and is the sole explicit exception needed to close a crashed
  assistant/tool role sequence.
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
- A caller-supplied session identifier that contains a configured sensitive value or
  one of its encoded forms is rejected with a constant diagnostic before session
  persistence or public rendering.

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

- [x] Reproducing the reported provider rejection produces one actionable report with
      a stable local incident code, provider, transport, redacted model, HTTP status
      when known, retry disposition, action, and separate session ID.
- [x] The report contains neither `provider-supplied detail omitted` as its primary
      diagnosis nor nested `agent run failed ... llm_stream_failed` prose.
- [x] T022's typed safe error reaches every applicable observer without string parsing
      or a competing error type.
- [x] Complete and streaming paths classify the same fixture identically for OpenAI,
      xAI/Grok, OpenRouter, Meta, Anthropic, Gemini, and Mock.
- [x] Authentication, rate limit, quota/billing, model unavailable, unsupported
      request, provider unavailable, transport, protocol, cancellation, and unknown
      rejection fixtures select only their documented deterministic action.
- [x] Unknown or malformed remote codes never become a more specific local class.
- [x] Raw, URL-encoded, JSON-escaped, and control-character forms of active/inactive
      credentials, prompts, endpoints, model labels, provider messages, and arbitrary
      metadata do not reach errors, debug output, streams, sessions, logs, CLI/TUI,
      MCP, or gateway output.
- [x] Request IDs appear only from provider-specific documented headers after ASCII,
      syntax, length, redaction, and output-bound checks.
- [x] HTTP bodies remain bounded and are discarded after safe classification; no debug
      or verbose mode exposes them.
- [x] Agent outcomes are stable machine values, structured failure evidence is additive
      and bounded, and legacy string-only sessions remain readable without inferred
      detail.
- [x] A provider failure cannot authorize a tool, alter plan blocking semantics, leave
      a continuation reusable, or be reported as success.
- [x] Typed provider failures, rendered incident reports, and provider rejection prose
      are never persisted as assistant-authored messages or supplied to a later model
      turn under the assistant role. T022's bounded interrupted-continuation role
      tombstone remains outside this provider-failure representation.
- [x] Chat remains usable after a safely reconciled provider failure; `nib run` returns
      nonzero; TUI, MCP, gateway, delegated, durable, planner, and compression paths
      preserve the same safe classification.
- [x] Interactive TTY output uses only the selected native renderer. Plain, redirected,
      `NO_COLOR`, test, persisted, MCP, and gateway output contains no ANSI escapes,
      control characters, `[red]`/`[dim]`-style tags, or interpreted user content.
- [x] Output is bounded under long provider/model/request-ID/session values and remains
      readable at narrow terminal widths.
- [x] `nib doctor` exposes the resolved local provider/transport/capability context
      needed by every recommended action without making a paid request.
- [x] User and technical documentation explain the safety boundary, incident codes,
      recovery actions, and where to find the session audit record.
- [ ] `task docs:check`, `task check`, `task test`, `task check:all-targets`,
      `task coverage`, `task build`, and `git diff --check` pass on the exact
      implementation revision.
- [x] Independent spec-compliance and security/quality reviews have no unresolved
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
CLI child-process redaction, and the user-guide recovery table remain in place. The
T022 typed `LlmRequest` change did not introduce a competing failure type. MCP `nib_run`
followed by `nib_get_status` now surfaces the persisted typed failure (incident class
and `LLM-AUTH` on a credential-rejected fixture) without the provider body. This change
set does not close T026: coverage, all-targets, raw-terminal recovery smoke, and
independent reviews stay open.

## Request-ID Safety Evidence (2026-08-23)

[OpenAI's API debugging reference](https://platform.openai.com/docs/api-reference/debugging-requests)
documents `x-request-id`, and
[Anthropic's error reference](https://platform.claude.com/docs/en/api/errors) documents
`request-id`. The implementation now captures only `x-request-id` for the canonical
`openai` provider across Chat Completions and Responses, and only `request-id` for the
native Anthropic Messages adapter. Shared codecs do not infer either header for
xAI/Grok, OpenRouter, Meta, or Gemini.

The allowlisted header is snapshotted before bounded error-body consumption, rejected
before allocation beyond 128 bytes, and passed through `LlmError::with_request_id` for
ASCII syntax validation and full configured-sensitive-value redaction. Local complete
and stream HTTP fixtures cover accepted IDs plus oversized, malformed, sensitive,
wrong-header, and cross-provider inputs. An accepted ID is included in the standard
bounded safe report; rejected values remain absent from the typed error and every
observer.

## Retry Evidence Propagation (2026-08-23)

T022's shared executor now attaches bounded actual-attempt metadata to both normalized
success and failure values: a zero-to-three attempt count, a boolean recording whether
a `429` advanced to a different configured credential, exhausted state, and an optional
validated final `Retry-After` value no greater than 30 seconds. The typed fields are
serialized directly and are not reconstructed from prose; Mock/local responses use the
explicit zero-network-attempt convention.

Final transient complete and initial-stream HTTP failures report `exhausted` or
`exhausted after credential rotation` from those facts. Private stream completions,
post-handshake protocol/transport failures, and immediate response validation failures
retain the same numeric attempt metadata through one bounded `LlmErrorContext` path.
Credential-free fixtures cover final `429` rotation, Anthropic `529`, bounded hints,
successful complete/stream retries, and a stream failure after a successful retry.
This evidence advances the retry fields in the safe failure model but does not close
T026's remaining observer, persistence, coverage, terminal-smoke, or independent-review
gates.

## Interactive and Gateway Observer Evidence (2026-08-24)

The redirected plain-chat child fixture now pre-creates and resumes the exact
`plain-recovery-session`, then serves three credential-free localhost Responses
requests: one structured planning failure, one recovered structured plan, and one
streamed assistant completion. Input supplies two user goals, approves the recovered
plan, requests `/status`, and quits. Both `NO_COLOR` dispositions run at `COLUMNS=24`
and `LINES=6`; their bounded failure reports and post-failure status semantics are
identical. The failure renders once, the later assistant success is authoritative, and
the persisted session contains exactly two relevant user messages and one successful
assistant message with no assistant-authored failure. Raw, percent-encoded, and Base64
credential forms, provider-body sentinels, ANSI/control bytes, and Rich-like tags are
absent from redirected output and the session JSON. The exact focused child test passed
1/1.

The real gateway handler now has deterministic localhost coverage for a typed `401`
failure followed by a successful Mock turn in the same gateway session. It returns one
bounded `LLM-AUTH` report, persists exact `planning_failed` / `authentication` /
`LLM-AUTH` fields without an assistant failure message, and remains usable for the
subsequent successful gateway request. The exact focused gateway test passed 1/1.

The existing MCP `nib_run` plus `nib_get_status` observer fixture now scans raw,
percent-encoded, Base64, markup, ANSI/control, and remote-body sentinels across the run
response, two status responses, the subagent record, and the child session. It asserts
the exact persisted `planning_failed` / `authentication` / `LLM-AUTH` schema, no
assistant-authored failure, and semantically identical repeated status. The exact
focused MCP test passed 1/1.

Ratatui's bounded failure-detail test keeps the exactly-once heading assertion on the
full safe timeline and verifies that a deliberately short, bottom-aligned TestBackend
viewport retains the actionable provider, HTTP/retry, and session tail without control
bytes or provider detail. Native raw-terminal recovery smoke, coverage, the complete
combined-tree gate set, and independent final reviews remain open; this evidence does
not move T026 or complete any acceptance checkbox.

## Durable Scheduled-Run Evidence (2026-08-25)

Scheduled agent failures no longer discard `AgentRunSummary.failure` while converting
the summary into a terminal durable-task result. The scheduled session event and the
corresponding bounded run entry now retain the stable machine outcome plus the same
serialized `LlmError`; rendered user-report prose is excluded from durable machine
state and daemon audit text. Legacy/local failures retain their redacted string path.
A deterministic owned-publication regression asserts the exact authentication class,
`LLM-AUTH` incident code, stable outcome, and absence of both an active secret and the
rendered failure heading across the durable record and session event. Interactive
ledger projection also allowlists the serialized `incident_code`. This closes one
local durable-observer gap only; delegated/compression breadth, native recovery,
coverage, exact combined gates, and final reviews remain open.

## Terminal Classification and Combined Local Evidence (2026-08-26)

`ProviderUnavailable` is now constructed only when the typed retry controller proves a
retryable transport/status exhausted its bounded attempt budget. A single transient
status cannot be promoted by deserialization or presentation code; legacy or malformed
metadata remains a conservative transport/adapter failure. Scheduled-run terminal
publication preserves the same typed error while exposing only the stable safe outcome
in daemon error text.

`task test:durable` passed 4/4, `task test:runtime-e2e` passed 16/16, and the focused
interactive, LLM conformance, offline live-harness, and release-qualification suites
passed. The combined `task check` and `task check:all-targets` gates were green, and
`task coverage` passed at 85.71% runtime line coverage (82,695 / 96,482). The Linux
optimized redirected/PTY smoke passed with terminal restoration and privacy assertions.

The optimized Linux PTY smoke now also injects one credential-free Mock authentication
failure under an exact smoke-only goal, renders exactly one bounded `LLM-AUTH` report,
then completes a later tool-backed turn in the same plain-chat session. Its authoritative
session retains typed `planning_failed` / `authentication` evidence plus the later
`completed` run, while the rendered report is absent from assistant-authored content.
This closes the raw-terminal failure/recovery gate on Linux. The clean exact-revision
gate and independent final reviews remain open, so T026 stays in Development until those
items are reconciled.

## Security Review Reconciliation (2026-08-26)

The first independent security/quality review identified five publication-boundary
gaps. The current tree now validates MCP status identifiers against every configured
and environment-derived sensitive value before profile resolution or audit; raw,
embedded, percent-encoded, JSON-escaped, and Base64 variants have deterministic
pre-persistence coverage. Provider stream deltas and planning deltas remain private
until terminal validation succeeds, and public events are derived only from the
authoritative completed response. Refused, incomplete, and late-failing streams expose
no partial content or tool proposal.

All plain, TUI, chrome/status, history, and one-shot presentation paths now apply
bounded control-safe projection with the complete public sensitive-value set. One-shot
goals are capped before persistence and are not repeated in startup output. Plain-mode
approval/question ownership is claimed atomically before broker delivery, and its
state-synchronized regression covers input arriving in the former claim-to-prompt
window without timing sleeps.

On this reconciled tree, `task fmt`, `task check:all-targets`,
`task test:interactive`, `task test:llm-conformance`, and the complete `task check`
passed. This evidence supersedes the earlier 85.71% coverage count only for source and
test state; a new exact-tree coverage run, optimized build/smoke, documentation check,
diff check, and both independent re-reviews remain open. Public provider content is
therefore terminal-authoritative even though adapters continue consuming the wire
incrementally; the user guide and architecture now state this security boundary
explicitly.

The first re-review then found three additional alternate-boundary classes, and the
spec re-review independently found the corresponding question/history gaps. Raw
`LlmStream::recv` is now restricted to `crate::llm`; application code and the live
qualification harness can only finish the incremental stream. Validated provider
content, tool intent, plan steps, questions/options, and question answers are redacted,
control-safe, and bounded before session/event persistence, while raw tool authority
remains private to the active execution path.

Plain/TUI command output, provider/model selection labels, model-change confirmation,
session rename, session previews, persisted activity projection, TUI detail, and live
status now all carry the full configured and environment-derived sensitive-value set.
Regression coverage includes raw, JSON-escaped, Base64, terminal-control, and bidi
forms across those surfaces. `task check:all-targets` and the expanded
`task test:interactive` passed after this reconciliation. The updated LLM conformance
suite, complete exact-tree gates, and both independent re-reviews remain open, so this
section records remediation rather than review closure.

The next exact-tree re-review found the remaining execution and durable-publication
edges. `ask_question` now executes only its bounded public question/options/answer
projection, including handler failures, and the real executor/session regression scans
the attempted event, tool-call record, result, and error together. Completed model
content is projected before plan-step outcome persistence, and compression projects
provider summaries before token truncation and compare-and-swap publication.

Every production `ToolExecutor` and MCP runtime constructor now receives the complete
public sensitive-value set, including registered provider environment credentials.
Executor results, arguments, terminal chunks, approvals, and audit records recognize
raw, percent, JSON, and Base64 spellings and neutralize terminal-active controls. MCP
startup errors and successful tools/list metadata use the same bounded encoded-secret
boundary, including the additional JSON serialization layer around already escaped
values. Oversized schema diagnostics retain only a bounded actionable prefix.

On this reconciled source tree, `task check:all-targets` passed and the complete serial
`task test` passed with 931 library tests plus all binary and integration tests; the
credentialed live qualification and optimized release-binary qualification remained
intentionally ignored behind their explicit authorized tasks. Documentation, coverage,
optimized build/smoke, and both fresh independent re-reviews remain open, so lifecycle
completion is not yet claimed.

## Final Redaction-Boundary Reconciliation (2026-08-27)

The final review cycle identified and closed truncation-order and alternate-spelling
gaps across activity/history previews, TUI detail, MCP and executor schema failures,
tool-result metadata, plain quit output, oversized `/diff`, and foreground terminal
results. Foreground terminal output is now projected before its retained tail becomes
the authoritative `ToolResult` and audit record; raw truncated streams fail closed
when any configured sensitive value is present, and failure prose is regenerated from
the projected stdout and stderr.

Live terminal projection incrementally decodes up to eight percent-encoding stages
while retaining source-byte provenance for benign text. Configured sensitive values
are expanded through the same bounded decode stages, so a credential containing a
percent escape matches both its original and decoded output spellings across arbitrary
chunks. A ninth decoding stage, configured-secret decode overflow, or more than 1 MiB
of pending source state fails closed. Regressions cover raw, one-pass, nested, decoded,
fragmented, truncated-tail, persisted-session, and oversized-diff cases while retaining
benign text such as `a%20b`.

On the reconciled implementation tree, `task check:all-targets`, the complete serial
`task test` (940 library tests, 80 binary tests, and all non-live integration targets),
and the aggregate `task check` passed. `task coverage` passed at 85.97% runtime line
coverage (84,717 / 98,537); `task build`, the offline Linux PTY and redirected
interactive smoke, `task docs:check` (5/5), and `git diff --check` also passed. The paid
live-provider qualification and optimized exact-release-revision qualification remain
behind their explicit authority and clean-revision gates.

Fresh independent spec-compliance and security/quality re-reviews both returned PASS
with no unresolved functional, high/medium security, or code-quality findings. T026
remains in Development solely because the combined exact-implementation-revision gate
cannot be claimed from the current dirty, uncommitted worktree; no commit or release
qualification was authorized in this work session.

## Open Questions

- Should the stable incident-code registry live beside T022's `LlmError` or in the
  presentation module? The code-to-class mapping must have one owner regardless of
  module placement.
