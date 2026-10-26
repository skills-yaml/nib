# T090: Preserve Tool History and Recover Read-Only Status Work

**Status:** Development — user authorized implementation on 2026-10-26.
State: development
Primary Feature: agent-runtime

## Problem and Evidence

A user requested `repo status`. nib repeatedly loaded the reporting skill and
queried Git, then stopped on a missing terminal scope without producing a report.
Source inspection at `b766d62` and a matching local session audit on 2026-10-25
establish the following. Private session identifiers, local paths and raw logs
are intentionally excluded.

| ID | Finding | Evidence and consequence |
| --- | --- | --- |
| P1 | Tool continuation loses earlier exchanges. | `src/agent/loop/inner.rs` skips context rebuilding when continuation is available. `src/llm/openai.rs` retains only the latest assistant call and results, appending them to the original messages. After Git status, the preceding skill result disappears from the next request. |
| P2 | Successful repetition is invisible to the failure guard. | `FailedToolBatchGuard::observe` in `src/agent/loop/support.rs` resets on success. The audit records four successful skill loads, three successful Git status calls, and repeated `already_active: true` results. |
| P3 | Repairable missing scope ends the run after an approval announcement. | `src/agent/loop/inner_exec.rs` emits `approval_required` before execution preflight, then routes all-missing-scope batches to reconciliation. The audit records approval announcement, scope rejection, terminal outcome and plan invalidation. |
| P4 | Verification ignores task applicability. | `add_independent_verification_requirements` in `src/agent/loop/verify.rs` adds `task verify` solely because that substring occurs in project instructions. Its SHA-256 prefix matches the displayed obligation `a8d4d23b6e776e81`. |

The audit confirms persisted successful observations, not their deletion from
storage. Request assembly is the defect. Context snapshots show ample headroom
and no compression requests. A deterministic outgoing-request regression remains
required before implementation can claim the diagnosis reproduced in tests.
Current configuration selects OpenRouter Chat Completions; that observation does
not prove which model was selected historically for every pasted transcript line.

Related historical contracts are [T041](../../done/agent-runtime/T041_task_aware_context_and_verified_completion.md),
[T054](../../done/tools-sandbox/T054_reconcile_local_preflight_failures.md),
[T060](../../done/tools-sandbox/T060_terminal_listing_instruction_preflight.md), and
[T068](../../done/llm-providers/T068_anthropic_stream_continuation_preservation.md).
This follow-up supersedes T060's immediate blocked outcome only for repairable
missing-scope proposals. It preserves historical done records and fail-closed
execution rules.

## Goals, Consumers and Non-Goals

Users of plain chat, TUI and the shared agent runtime must get continuous,
bounded progress and a useful report or concrete blocker. Preserve complete
provider history, recover invalid proposals before execution, and apply only
verification obligations appropriate to the work.

No new framework, database migration, skill-policy rewrite, blanket terminal
approval, automatic scope broadening, paid provider calls or historical session
repair. Do not change `/model` availability during active execution: it is an
explicit busy-state restriction, independent of the model-picker fix.
The empty-queue message is an unverified adjacent issue. Investigate its initiating
input in a separate follow-up if reproduced; do not infer an empty Enter bug or
change queue behavior here. Ordinary empty submission is already guarded.

## Proposed Behavior and Fixes

### F1: Complete, bounded continuation history (P1)

The provider adapter owns a cumulative native continuation transcript for one
provider/model/transport/session/run. Each request includes every earlier exchange
in that continuation exactly once, plus each newly resolved call/result pair.
The runtime must not combine a stale base prompt with only the newest pair.
Preserve native IDs, order, parallel-call grouping, opaque metadata and private
reasoning/signatures where the adapter requires them. Preserve the existing
single-use and scope validation rules. Do not serialize private native content
into user-visible messages, debug logs or persisted public audit records.

Audit Chat Completions, Responses and Gemini for the same single-turn assumption;
fix affected adapters and use Anthropic's cumulative-history behavior as existing
evidence, not as permission to copy its wire representation. Cover all adapters
through the common contract, including adapters that already comply.

Count cumulative bytes/items and complete outgoing request tokens. At a bound,
use the existing safe rebuild/compaction path only after resolving pending calls
and preserving required evidence; otherwise stop with an actionable bounded
outcome. Never silently drop unresolved pairs or required private protocol state.
Refresh changed skill/instruction/plan context through a deliberate request
boundary without duplicating or losing exchanges. Cancellation, model switching
between runs and steering retain existing continuation invalidation semantics.

### F2: Detect successful work that makes no progress (P2)

Add a separate semantic no-progress guard; do not redefine successful tools as
failed. Track canonical tool name/arguments and sanitized semantic observations,
ignoring invocation IDs, elapsed time and other volatile metadata. Detect repeated
identical batches and the observed alternating two-batch cycle. After three
occurrences of an unchanged batch or cycle without meaningful evidence or plan
progress, inject one corrective instruction identifying the repeated work and
asking for the next missing action or a report. If the same cycle completes again
without progress, reconcile with a concrete blocker and available partial findings.

New evidence, a genuinely advanced plan step, human steering, or a tool's declared
poll/wait contract breaks the cycle. New call IDs, repeated commentary and repeated
loading of an active skill do not. Keep tracking bounded and scoped to the run.
Do not escalate to a paid/stronger model merely because a tool repeats successfully.

### F3: Validate proposals, then approve execution (P3)

Perform side-effect-free argument/scope preflight before emitting an approval
request. For omitted, empty or structurally invalid `affected_paths`, return a
structured tool observation describing the invalid argument and permit up to two
corrected proposals under the same plan and step. The model must declare the
intended worktree-relative scope; the runtime must not silently insert `.`.
Each corrected proposal receives a fresh invocation identity and full scope,
instruction, verification and permission checks before execution.

Do not execute or request approval for the invalid proposal. Independent valid
reads in a mixed batch may complete once and their results must survive repair;
never replay them or completed mutations as a side effect of retry. Keep the
plan active during repair and terminally block only when the repair budget is
exhausted or a non-repairable condition is reached. Approval denial, unsafe or
outside-worktree paths, unavailable instructions and failed worktree preparation
retain fail-closed behavior and are not bypassed by this recovery allowance.
Both plain and TUI must clear stale approval UI on rejection/cancellation and
render the same repair or terminal state after reloading the session.

### F4: Apply verification by authority and task scope (P4)

Replace substring-only verification inference with an applicability decision
bound to the authoritative request/plan and its permitted effects. For explicitly
read-only reporting with no executed mutations, implementation-completion commands
mentioned in project policy do not become mandatory just by appearing in text.
Explicit human requests to run a verification command remain obligations, and
project requirements that explicitly apply to reporting still apply. Quoted
examples and negated instructions alone cannot create an execution obligation.

Record why an obligation is included or excluded. Do not trust an arbitrary model
label to waive verification: reconcile against requested operations and actual
mutation evidence. A report-only plan proposing a mutation must stop for scope
resolution, not silently reclassify the request. For authorized implementation
plans, retain required gates and invalidate stale evidence after mutations.
Conflicting or genuinely ambiguous instructions require clarification; no edit to
AGENTS.md or governed policy is authorized by this spec.

## Model Delta and Invariants

This is a bounded runtime contract delta, not a new workload domain or schema.

| Invariant | Owner and representation | Consumers and failing scenario |
| --- | --- | --- |
| I1: exchanges appear once, in order, within their bound run | Adapter-private continuation; public session audit remains authoritative for executed effects | Request builder and token budget; third request loses first tool result (AC-1/2) |
| I2: success is distinct from meaningful progress | Run-local semantic guard; existing plan-step state remains authoritative | Agent loop and escalation; successful alternating cycle never terminates (AC-3) |
| I3: invalid proposals have no execution or approval authority | Preflight observation and existing invocation IDs; corrected calls are new proposals | Executor, permissions, verification, plain/TUI; missing scope displays approval and invalidates plan (AC-4/5) |
| I4: verification has authority and applicability | Existing plan obligations and audit rationale, derived from trusted scope/effect evidence | Planner and completion reconciler; read-only status requires unrelated test execution (AC-6/7) |

No persisted schema fields change. Additive string events `tool_scope_repair`,
`tool_progress_correction` and `report_scope_conflict` use existing event envelopes;
terminal outcomes add `repeated_tool_no_progress` and `report_scope_conflict`.
Current plain/TUI live and reload reducers classify both as failures and retain
partial findings in the public transcript. Older readers accept unknown strings
but cannot supply the new specialized recovery presentation; use the current
binary to resume/review these sessions. Existing outcomes and saved sessions keep
their meanings. No private native continuation is persisted. This is a compatible
patch, with no migration or rewritten historical evidence.

## Affected Areas and Delivery Order

1. Provider codecs (`src/llm/{openai,responses,gemini,anthropic}.rs`), continuation
   types and adapter fixtures; context snapshot/budget consumers.
2. Agent context rebuilding, skill activation, semantic progress tracking and
   escalation guards under `src/agent/loop/`.
3. Tool preflight/approval ordering, plan reconciliation and verification setup;
   shared interactive reducers and plain/TUI approval rendering.
4. Runtime integration tests, Task-owned focused entrypoints if missing, user
   architecture documentation, spec catalog and memory at implementation closure.

First reproduce history loss with a local scripted provider, then fix F1, then
F2, then F3/F4 and shared consumer behavior. Retain separate commits where useful,
but qualify their combined behavior before delivery. No paid live test is needed
to establish outgoing request correctness or deterministic recovery.

## Acceptance Criteria

- [x] AC-1: A scripted provider requires at least three successive tool batches
  (skill load, Git status, another inspection) and a final response. Every later
  outgoing request retains all earlier pairs exactly once; the report completes.
  Assert request bodies, not merely final wording, in streaming and complete modes.
- [x] AC-2: All supported adapters satisfy cumulative history, parallel calls,
  error results, scope mismatch, cancellation/steering, private metadata and
  cumulative budget boundaries. Skill and plan updates remain visible. Fixtures
  prove no unauthorized tool executes after malformed/truncated provider output.
- [x] AC-3: Repeated successful identical and alternating two-batch cycles receive
  one corrective opportunity and a bounded terminal outcome if unchanged. New
  evidence, legitimate polling and real plan progress do not falsely trip it.
- [x] AC-4: An opaque terminal command with missing scope produces no approval
  request or shell execution, retains its plan and receives a repair observation.
  A corrected scope executes once after normal authorization; repeated invalid
  proposals stop after two repair attempts with actionable partial findings.
- [x] AC-5: Mixed-batch recovery preserves successful calls exactly once. Denial,
  outside paths, missing instructions and cancellation remain fail-closed. Live
  and reloaded plain/TUI states show no orphaned approval or falsely completed step.
- [x] AC-6: `repo status` with a report-only contract and project implementation
  gates creates no unrelated `task verify` obligation and runs no tests. An explicit
  human verification request and applicable project reporting gate remain enforced.
- [x] AC-7: Implementation plans retain their required gates; mutations invalidate
  prior evidence. Negated/example text does not create obligations, model-supplied
  read-only claims cannot waive them, and ambiguous applicability is surfaced.
- [x] AC-8: Offline end-to-end status reproduction loads the skill once, preserves
  observations, repairs a missing scope and delivers a report with limitations.
  The no-progress fixture terminates usefully instead of announcing repeated intent.
- [ ] AC-9: Independent review and all required native gates pass on the frozen
  candidate; actual integration, lifecycle, version and memory records reconcile.

## Validation Gates

For this specification-only change: independent contract review, `task docs:check`
and the repository-required `task verify`. Passing these validates the artifact,
not implementation acceptance criteria.

For implementation, add red regressions first and run in dependency order:

1. Adapter and private continuation fixtures through `task test:llm-conformance`;
   add a Task-owned multi-round target if existing selection misses new tests.
2. Context/budget/progress/verification consumers through `task test:agent-context`.
3. Preflight and authorization through `task test:terminal-preflight`, then shared
   UI consumers through `task test:interactive` and `task test:tui-shutdown`.
4. Combined scripted-provider flows through `task test:runtime-e2e` and applicable
   release qualification/smokes from the gate map. Verify native platform policy.
5. After fixes, documentation and independent exact-candidate review stabilize,
   run fresh `task docs:check`, `task versions:check`, and `task verify` (includes
   check and test once). Qualify actual shared integration and main revisions.

A failed prerequisite blocks dependent claims; absent fixture coverage is a gap.
Do not count today's source analysis as a passing regression. No cached-result
reuse or reduction of existing sandbox, privacy or release gates is authorized.

## Risks, Compatibility and Rollback

Cumulative history increases request size: measure it and preserve existing hard
limits. Native private metadata must remain private during rebuild and diagnostics.
Repair cannot replay mutations or grant approval. Progress detection may mistake
polling for looping; explicit polling fixtures and evidence-sensitive reset rules
are required. Applicability decisions must not weaken implementation verification.

Prefer internal changes and existing public interfaces. Rollback reverts runtime
changes without deleting sessions or rewriting audit history; it restores the
known defective behavior and is not a data-recovery mechanism. Never decrement or
reuse a published release. Main merge and publication are separate events.

## Version Impact

Implementation is authorized and the development-start reservation is applied. Move
to test only after confirmed integration into `development`; done requires verified
`main` delivery and reconciled acceptance, documentation, versions and memory.

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-status-recovery | Compatible runtime continuation and recovery corrections; fresh 0.7.1 from published 0.7.0, applied once at development start by t090-author. |

Do not append to released 0.7.0. Recheck current publication and shared reservations
at handoff; t090-author owns the single 0.7.1 bump.

## Memory Impact

Status: updated
Rationale: The implementation contract and separate T023 live-qualification boundary
are recorded in [workspace/agents/memory/decisions.md](../../../agents/memory/decisions.md)
and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md);
delivery evidence remains tracked here.

## Implementation Start (2026-10-26)

The user authorized implementation. The development manifest binds published
0.7.0 to b766d62; the fresh patch reservation is nib-status-recovery, target 0.7.1.
The peer runtime refuses an uninitialized board; preserve existing ownership
and use the established single-author ledger fallback. One author writes code
in an isolated worktree; supporting design/review agents remain read-only.
No paid provider call is authorized by this implementation record.

## Implementation Evidence

The initial outgoing-history regression failed on Chat Completions at the third
request, reproducing P1. The corrected fixture covers all ten network adapter
configurations in both complete and streaming modes, with four parallel-call
rounds, successful/error outputs and escaping-heavy content. It checks retained
prefixes, exactly-once receipts, conservative native replay sizing and bounded
admission. Native cumulative byte/item fixtures reject overflow without exposing
private content.

Runtime fixtures exercise a loaded skill in the next request's system context,
the original compound Git status/worktree command, malformed scope repair without
replaying a successful read, three independent repairs, repeated invalid scope,
read-only mutation rejection, and a no-progress correction followed by useful
partial findings. Unit fixtures cover alternating cycles, evidence/step/poll resets,
quoted and negated verification text, command boundaries and ambiguous applicability.
Interactive fixtures compare live and reloaded failure presentation.

AC-1/2 are supported by `test:t090-history` and `test:llm-conformance`;
AC-3 by the semantic guard and scripted no-progress fixtures; AC-4/5 by
`test:t090-runtime`, `test:terminal-preflight`, `test:interactive` and
`test:tui-shutdown`; AC-6/7 by applicability, exact-command authority and mutation
invalidation fixtures; AC-8 by the scripted status fixture. The legacy coding
fixture retains wrapped Cargo testing under a non-exact request; a separate
success/unresolved fixture enforces explicit human `task check` exactly.
Report scope also rejects repository executables masquerading as read-only
utilities, including compound segments.

Independent review of 847fb8e approved the adapter/spec slice and identified one
runtime scope finding; the subsequent path-qualified executable rejection resolves
it. Final exact review, full native gates and shared integration remain required
for AC-9. No live-provider qualification is inferred.
