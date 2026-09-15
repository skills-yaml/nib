# T042: Context Budgeting and Live Visibility

**Status:** Backlog

**User decision (2026-09-15):** A compact live context indicator with an on-demand
`/context` breakdown, rather than a permanently open detailed panel.

**Related:** [T041: Task-Aware Context and Verified Completion](../development/T041_task_aware_context_and_verified_completion.md),
[T003: Context Engine](../done/T003_context_engine_with_dynamic_compression_and_session_management.md),
[T022: Provider Contract](../done/T022_provider_neutral_llm_contract_and_adapter_conformance.md),
[T032: Explicit Compaction](../done/T032_ft019_explicit_compaction_and_session_background_commands.md),
[T023: Live Qualification](../development/T023_live_llm_provider_model_integration_qualification.md)

## Objective and Scope

Preserve the information nib needs to solve the user's problem while reducing
unnecessary model input, repeated reads, and compression calls. Let the user inspect
context at any time, including during generation, tool execution, compaction, and
approval or clarification waits. Inspection must be local and read-only.

This spec owns complete request accounting, budget allocation, safe compression
coverage, tool-schema preservation, continuation accounting and evidence retention,
usage reporting, and live monitoring. T041 owns the meaning and provenance of human
intent, scoped instructions, skill relevance, and verification obligations. T042 must
consume those contracts rather than create a second task or permission model.

The source audit is pinned to development commit `bceb3eb`. T040's reviewed local
implementation, `4234be8` on `feat/t040-resourceful-agent-loop`, is not integrated at
that revision. It fixes attachment inclusion/reads, the initial history/compression
threshold mismatch, summary output bounds, and some instruction/history retention.
Integrate and re-audit those changes before dependent implementation; do not count
them as shipped behavior or duplicate their implementation in this spec.

## Source Review and Findings

The following are source findings, not results from live-model evaluation. The
multi-round adapter issue is explicitly an unexecuted inference requiring a regression
fixture. No token savings percentage has yet been measured.

| Finding | Evidence at the audited revision | Consequence |
| --- | --- | --- |
| The displayed percentage is not the request footprint. | `src/interactive.rs::persisted_context_usage` measures bounded history; `format_tui_interaction_chrome` divides it by the full configured window. | Instructions, tools, native continuation, and response allowance are absent; the display may mislead in either direction. |
| Inspection is unavailable at important moments. | `/status` is `RequiresIdle`; no interactive `/context` exists. `src/context_cmd.rs` prints assembled project/profile context, not a live session request. The narrow TUI status fallback omits context. | Users cannot consistently inspect context while work is active or the terminal is narrow. |
| Input can consume the whole configured window. | `src/context/budget.rs` bounds input against `context_length`; planner and loop requests leave `max_output_tokens` unset. | No coordinated room is reserved for generation or reasoning. |
| Continuations bypass normal budgeting. | `src/agent/loop.rs` skips rebuilding/compression while a provider continuation exists. `src/llm/types.rs` caps continuation at 256 items/4 MiB, independently of the token window. | The last `context_bounded` event becomes stale; a tool chain can exceed the intended request allowance. |
| Earlier native tool rounds may disappear. | The loop reuses base messages while Chat, Anthropic, and Gemini replacement continuations store the latest response. Responses retains an accumulated replay tail. | Source tracing suggests different retention behavior across adapters; existing native runtime tests cover only one tool round after planning. |
| Compression can claim unseen coverage. | `compression.rs` concatenates the intended prefix; `bound_single_turn_input` can remove its middle; publication still advances `summary_index` across that prefix. | Required evidence can disappear from working context without ever reaching the summarizer. Raw history remains available, but the coverage claim is misleading. |
| Tool compaction changes schema meaning. | `budget.rs::strip_schema_annotations` removes annotation-named keys at every nesting level, truncates arrays to 64, and oversized/compact schemas become permissive objects. | Legitimate properties such as `description`, enum values, and required fields can be lost, causing incorrect calls and retries. |
| Fixed slices can waste usable capacity. | Runtime allocations start at 45% context, 30% tools, 25% history; group/section slices and tool ordering are not task-sensitive. | Content can be truncated despite unused space elsewhere. Unselected tools have no general model-facing recovery path. |
| Consumption is not surfaced. | `LlmResponse.usage` already contains validated input/output and optional cached/reasoning counts, but loop, planner, and compression do not consume it. | Users cannot distinguish request occupancy from cumulative consumption or see the cost of compression. |
| Monitoring can be both expensive and stale. | The TUI loads the session before checking its chrome cache on each render loop; the cache key has no context revision. | Long histories add local I/O, while the rendered estimate can remain unchanged as context changes. |

Relevant sources: [context budgets](../../../src/context/budget.rs),
[compression](../../../src/context/compression.rs), [agent loop](../../../src/agent/loop.rs),
[provider types](../../../src/llm/types.rs), [shared interaction](../../../src/interactive.rs),
[TUI](../../../src/tui/mod.rs), and [context CLI](../../../src/context_cmd.rs).

## Required Behavior

### 1. One request snapshot for accounting and inspection

Create a bounded, versioned context snapshot from the request actually prepared for
dispatch. Bind it to project/profile, session, run, request sequence, provider/model,
transport, phase, and context revision. Phases include planning, execution,
continuation, and compression. Track prepared, sent, completed, failed, and abandoned
states so a preview cannot masquerade as the last sent request.

Include:

- Effective configured window, its source, input allowance, response reserve,
  uncertainty margin, estimated input, and remaining headroom.
- Contributions from instructions, current goal/step, skills, docs/attachments,
  memory, summaries, recent messages, tool observations, tool schemas, native
  continuation, and serialization overhead. Categories must not double-count content.
- Bounded source identifiers, included/available counts or ranges, and explicit
  reasons for selection, summarization, truncation, deferral, or omission.
- Estimate method/version, timestamp, freshness, last compression result, and any
  unknown contribution. Unknown must never be represented as zero or an exact total.

The finalized adapter must account for native wrappers and private continuation
inside its existing trust boundary. Publish numeric footprint metadata, not raw
provider items or reasoning. A generic JSON estimate alone cannot claim to measure
every transport's serialized request.

Retain a bounded latest snapshot and bounded history of aggregate changes through the
existing session ownership/persistence mechanisms. A compact derived projection may
support concurrent CLI reads, but it must be identity-bound, atomic, rebuildable, and
unable to grant authority. Old sessions with no snapshot show unavailable historical
accounting; do not reconstruct fictitious exact usage from their text.

### 2. Budget the complete request and leave response room

For each request enforce, before network dispatch:

```text
estimated input + response reserve + uncertainty margin <= effective window
input allowance = effective window - response reserve - uncertainty margin
```

The response reserve includes reasoning when the adapter's output ceiling counts it.
Use the existing output-limit capability with documented provider semantics; do not
silently disable reasoning or shrink the task's required output merely to make input
fit. Treat an output-limit terminal result as incomplete when the outcome requires
more work. Record why a request cannot be admitted.

The configured window is not proof of a model's actual capacity. Use validated known
limits where available, label configured-only values, and do not infer limits from
model names. The current four-characters-per-token estimate is neither an exact
tokenizer nor a safe universal upper bound. Version the estimator, test code,
multilingual text and escaped JSON, expose uncertainty, and use a conservative margin.
Any opaque contribution needs a documented conservative allowance; otherwise report
that admission cannot be established. Provider-reported usage can calibrate future
estimates, but must not relabel an earlier estimate as exact.

A provider context-limit rejection must retain the plan and evidence, expose the
estimate mismatch, and avoid resending the same oversized request. Any corrective
attempt needs a changed, admissible request and the existing bounded retry policy;
otherwise stop with an actionable result.

Preserve all selected required content when it fits the input allowance. Redistribute
unused optional allocations deterministically before truncating useful material.
Under pressure, preserve T041's protected intent, active constraints, unresolved
questions/checks, current step, and necessary recent tool evidence before optional
background material. If the protected minimum cannot fit, explain the blocker and
offer scope reduction, a larger configured window, or a new task; do not silently
discard it and continue.

### 3. Save tokens without weakening tools or losing evidence

- Reuse unchanged source fragments by identity and scope. Load relevant sections and
  bounded snippets instead of repeatedly sending whole documents or logs. Preserve
  provenance when equivalent content is deduplicated; textual similarity alone must
  not merge conflicting sources or different instruction scopes.
- Keep the original audited result and a bounded model-facing observation separately
  where the existing store permits it. Preserve errors, exit status, relevant output,
  paths and evidence identifiers. Give a scoped retrieval reference for omitted detail
  only when it really remains available; otherwise mark it unrecoverable.
- Preserve the validation meaning of every executable tool schema. Optional annotation
  removal must understand schema locations and preserve properties whose names happen
  to be `description`, `title`, `default`, or `examples`. Never truncate constraints,
  enums, required lists, or nested schemas into a different contract to save tokens.
- If a full required tool schema cannot fit, defer it visibly or block that operation.
  A deferred tool needs an explicit, bounded discovery/selection path before this
  optimization can ship. Do not pretend such a path already exists. Select tools for
  the approved step with deterministic tie-breaking, not alphabetical luck.
- Local caching reduces local work, not necessarily billed input. Stable prompt
  prefixes may help provider caching, but report cache benefit only from supplied
  usage evidence and do not send irrelevant content to chase cache hits.

### 4. Compress with honest coverage and bounded cost

Trigger maintenance from the projected next request's pressure, including summary,
selected sources, tools, continuation and response reserve. A history-only percentage
is insufficient. Prefer inexpensive omission of optional/recoverable material before
paying for a summary, while retaining the protected minimum.

Every raw range marked summarized must actually have been supplied to the summarizer.
For an oversized prefix, process bounded contiguous chunks and advance the coverage
cursor only through successful covered chunks. Do not head/tail-truncate a combined
source and then claim its missing middle was summarized. Preserve existing raw history,
compare-and-swap publication, concurrent-steering rejection, and audit identity.

Use a concise handoff of goal, constraints, decisions and their sources, unresolved
questions, verified progress, failures, evidence references and next work. Keep
protected task facts outside reliance on a lossy narrative alone. Structural coverage
does not prove semantic summary quality; test both coverage and task-critical recall.

Cap summary input/output and the number of chunks per maintenance episode. Use a
high-water/low-water policy with a minimum useful reclaim target, and do not summarize
an unchanged prefix again. Record generation cost, before/after request footprint,
coverage, and why maintenance ran, was deferred, or made insufficient progress.
On failure retain the last valid view and raw history. Never use a failed or stale
summary, enter a retry loop, or dispatch a request that cannot be admitted.

Keep explicit `/compact` as a separate controlled operation under T032. Reading
`/context` must never trigger compression. Compaction while a worker is active must
not become a second concurrent writer through this monitoring feature.

### 5. Preserve and budget multi-round continuations

Before every continuation request, account for its actual replay state. Preserve
required earlier tool evidence, call/result correlation and chronological order
exactly once across multiple tool rounds and all supported transports.

At pressure boundaries, permit compaction/rebuilding only after completed tool results
are durably recorded and no tool invocation is unresolved. Rebuilding must preserve
the approved plan, completed effects, steering, and recovery evidence; it must not
repeat completed tools or persist private continuation content. This is a reviewed
provider-contract change, not a text-only reconstruction of opaque reasoning.
If a transport cannot safely rebuild, reconcile with an actionable context-limit
result before sending an oversized request. Keep existing item/byte caps in addition
to the new request budget.

### 6. Compact indicator and `/context` at any moment

The TUI continuously shows a small, estimate-labelled occupancy indicator, such as
`ctx ~18k/64k`, with a text pressure state when useful. Its meaning is the latest
prepared/sent input footprint against the effective window; `/context` explains the
response reserve and usable headroom. Preserve an abbreviated `ctx ~28%` or `ctx ?`
at narrow widths instead of dropping the field. Percentages use the same snapshot
and denominator as the detailed view. During transitions label preparing, compacting,
stale, or unavailable state; do not animate fictitious token counts.

`/context` provides a bounded breakdown of the snapshot, contributors and omissions,
reserve/headroom, estimator accuracy, latest compression, and usage totals. It stays
available while generating, executing tools, compacting, waiting for approval or an
answer, cancelling, and idle. Use a presentation-neutral read-only action independent
of the active worker and make it reachable when a modal owns input. Opening/closing
it preserves the pending approval/question, composer draft, selection and focus; it
cannot answer a question, authorize an action, steer, or queue another agent turn.
The indicator remains visible alongside that inspection view.

Plain mode exposes the same `/context` command during active work, with concise
updates at request/pressure changes rather than continuous terminal spam. Add a
session-specific inspection form to `nib context`, including bounded JSON output for
a second terminal or redirected clients. Preserve the current project-context command
form for compatibility and clearly distinguish that preview from a live request.
Exact CLI flags and the modal-safe TUI binding are design decisions before development.

Monitoring uses the latest published snapshot, not a rebuilt prompt or a full session
scan on every frame. Target visible snapshot updates within one second and local
inspection within 250 ms in deterministic performance fixtures. Repeated inspection
causes zero model/tool calls, zero session mutations, and no wait for the run lease.
On contention or unavailable data, show the last snapshot with its age or a bounded
unavailable result; do not hang the UI or show another session's snapshot.

### 7. Separate context occupancy from token consumption

Consume the existing validated provider usage for planning, execution, continuation
and compression. Track per-request, per-run and session totals, including observed
retry/maintenance usage where available. Deduplicate by request/attempt identity and
validated terminal state; stream totals must not be added once per chunk.

Input and output totals include their optional cached-input and reasoning-output
subsets. Show those as subsets, not extra tokens. Missing provider usage, failed
attempt usage and unavailable cache details remain unknown/partial; estimates are
shown separately. Cumulative consumption is not current context occupancy, and
provider token usage is not a billing receipt. Price conversion is outside scope.

Default monitoring exposes bounded counts, state and sanitized source labels, not
raw prompts, secrets, tool results, or private reasoning. Scope every snapshot and
reader to the selected project/profile/session. Reject malformed or mismatched
projections and preserve existing output-redaction boundaries.

## Acceptance Criteria and Regression Matrix

All scenarios are planned. Each row requires observable assertions in production-path
fixtures, not success inferred from the model's final sentence.

| ID | Scenario | Required evidence |
| --- | --- | --- |
| C01 | Fresh planning/execution, compression, native continuation, small window, code/Unicode/JSON. | Captured prepared payload contribution totals reconcile; response reserve/margin apply before every dispatch; estimates/unknown values are labelled. |
| C02 | Three or more execution requests after planning, with two distinct tool rounds on every transport. | Final behavior requires both rounds; captured bodies preserve needed evidence once, correct identities/order, and no duplicate side effects. |
| C03 | Oversized native replay, provider context-limit rejection after local admission, and output-limit response. | Safe completed-tool rebuilding or explicit blocked result; retain evidence and expose estimate mismatch; no unchanged oversized retry, replay of completed effects, budget bypass, or false completion. |
| C04 | Large compression prefix with required evidence in the middle. | Every summarized range reached the summarizer; required facts remain usable; raw history and CAS survive concurrent steering/failure. |
| C05 | Already-small, unchanged, low-reclaim and repeatedly growing histories. | No redundant summary call; episode ceilings/hysteresis hold; summary cost and net reclaim are recorded. |
| C06 | Tool properties named like annotations, long enum/required arrays, deep and oversized schemas. | Exposed schemas retain validation semantics; deferred tools have a tested recovery path; required tool in alphabetical middle remains usable. |
| C07 | Underfilled groups, repeated files/logs, conflicting source scopes, required facts under pressure. | Unused capacity is reused; redundant optional input falls; protected evidence and provenance survive; omitted detail is actually retrievable or marked unavailable. |
| C08 | Indicator and inspection during every lifecycle state, at narrow widths, and with approval/question docks. | Same snapshot/denominator; inspection remains reachable; modal/draft state is preserved; no mutation, queueing, or model/tool call. |
| C09 | Large retained audit history, unchanged frames, concurrent readers and lock contention. | Snapshot render/lookup cost is bounded independently of transcript length; update/read latency targets hold or a bounded stale/unavailable result is shown. |
| C10 | Usage complete/missing/partial, streaming duplicates, retry, cache/reasoning subsets, and compression. | Totals do not double-count; unknown is not zero; occupancy is separate from cumulative usage; restart preserves truthful accounting. |
| C11 | Session/profile switch, stale run event, old session, secret/control-text marker, private continuation. | No cross-session projection, leaked content or fabricated historical usage; original CLI form remains compatible. |
| C12 | Identical problem-solving fixtures before/after optimization. | Same required facts, expected code artifacts, tests and plan reconciliation; count all model/tool attempts, rereads, compression, output and failures. |

- [ ] C01–C07 pass with captured request and artifact evidence.
- [ ] C08–C11 pass in shared reducers, plain CLI and native TUI qualification.
- [ ] C12 shows strictly less total estimated input in designated redundant-document
      and repeated-log fixtures without extra generation/tool rounds or reduced task
      success. Record actual usage where available and report any cost tradeoff.
- [ ] Baselines use a correctness-preserving reference: losing evidence or weakening
      a schema cannot make the old implementation a desirable lower-cost target.
- [ ] Source/context metadata, compatibility, two-stage review and required gates pass;
      live-model quality claims remain separately qualified under T023.

## Affected Areas and Implementation Plan

Affected areas are `src/context/`, agent loop/planner, provider request preparation and
usage, session projections, shared interactive commands, plain/TUI presentation,
`context_cmd`, configuration, regression fixtures, Task targets and user/technical docs.

1. Reconfirm the integration baseline and T040/T041 boundaries. Reproduce the suspected
   multi-round loss with captured native requests and record fixed benchmark inputs.
2. Design the versioned snapshot and full-request admission contract. Add adapter
   accounting and usage capture, then the read-only indicator and inspection paths.
3. Fix schema/coverage and continuation correctness before claiming optimization gains.
4. Add deterministic allocation, reuse and compression-pressure policy in reviewable
   slices; measure complete scenario costs and necessary-fact retention.
5. Qualify lifecycle/modal/second-terminal monitoring, legacy compatibility and native
   rendering; complete spec compliance review followed by quality review and gates.

Use child development specs if needed. This backlog spec does not authorize starting
runtime implementation, publishing a release, or running paid qualification.

## Validation Gates

While implementing, use `task check`, `task test:runtime-e2e`,
`task test:llm-conformance`, and `task test:interactive`. Add narrowly scoped Task
targets for the context snapshot, compression coverage and performance fixtures;
do not claim those new targets exist yet. Reuse T040's focused targets after integration.
Completion requires `task verify`, `task docs:check`, `git diff --check`, and relevant
native terminal/adapter gates. All ordinary fixtures are credential-free and bounded.

Spec authoring uses documentation integrity and source review. Deterministic tests
can prove accounting, coverage, protocol behavior and artifacts; live problem-solving
quality and provider cost need exact-revision, budget-authorized T023 evidence.

## Risks and Decisions Before Development

- Choose the snapshot storage/projection and retention limits, schema/defaults, invalid
  state handling, and old-binary/rollback behavior without adding an authority database.
- Specify output reserves, estimator margin and pressure hysteresis per request phase;
  benchmark latency and token cost before selecting final defaults.
- Review safe continuation boundaries for each transport. Unsupported rebasing must
  block honestly rather than drop opaque state or replay effects.
- Specify the bounded deferred-tool selection/recovery contract and required-content
  metadata with T041; neither may bypass tool permission checks.
- Select modal-safe inspection controls and exact CLI session/JSON flags, preserving
  existing input semantics. Monitoring performance must not slow active execution.

Non-goals include a permanently open panel, external dashboards/telemetry, monetary
billing estimates, an embedding/vector database, raw reasoning inspection, automatic
model switching, permission relaxation, and universal claims of lossless summaries.
