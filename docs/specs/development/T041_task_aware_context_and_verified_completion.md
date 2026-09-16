# T041: Task-Aware Context and Verified Completion

**Status:** Development

**Current stage:** Reconciled and implementation-ready; production changes and
regression tests are pending.

**Open design decisions:** None. Any change to the decisions below must update this
spec before its dependent implementation changes.

**Prerequisite:** Satisfied. T040: Resourceful Agent Loop and Context is complete at
`4234be8`, merged by `44c301f`, and verified on the integrated `development` baseline
`c64f953`.

**Related:** [T003: Context Engine](../done/T003_context_engine_with_dynamic_compression_and_session_management.md),
[T005: Runtime Lifecycle](../done/T005_full_runtime_state_machine_and_lifecycle.md),
[FT-006: Skills Management](../done/ft_006_skills_management.md),
[T023: Live Model Qualification](../development/T023_live_llm_provider_model_integration_qualification.md)

## Objective

nib should finish useful work with proportionate effort: inspect available evidence,
ask for information that materially affects the result, implement authorized changes,
and verify the outcome. It should retain the user's intent as context changes and
avoid extra model calls, irrelevant instructions, repeated questions, and unsupported
completion claims.

This spec covers the remaining findings from the T040 review. The verified integrated
T040 baseline is `c64f953`. This spec entered development on 2026-09-15 for design and
test preparation and was reconciled on 2026-09-16 after T040 integration. The design
decisions below are now authoritative for implementation. Source implementation remains
pending; live-model qualification remains separate under T023.

## Scope

Development covers the five behavior contracts below and their deterministic
regression tests. Implement them in reviewable slices, adding each production change
with its persistence/compatibility contract and focused tests. The resolved decisions
in this spec constrain those slices; changing one requires updating the spec before
dependent code changes. The development state does not claim implementation or tests
are complete.

## Findings and Existing Baseline

T040 adds shared behavioral instructions, bounded attachment inclusion, earlier
compression, a latest-user-role history reservation, repeated-failure termination,
blocked-step completion rejection, and correct Git worktree build dependencies.
Those changes and their deterministic validation are now part of `development` and
form T041's implementation baseline.

| Remaining finding | Consequence | Required outcome |
| --- | --- | --- |
| History reserves the latest `user` role, including synthetic continuation messages. | Runtime text can displace a human correction. | Identify message origin and preserve actual user intent. |
| One nearest project instruction file is loaded at startup. | Ancestor rules, nested rules, and later changes can be missed. | Load applicable instructions with scope, precedence, and refresh rules. |
| Automatic skill selection uses broad description-word overlap. | Irrelevant prompts, policies, and hooks can consume resources or affect execution. | Explainable, bounded selection tied to the task. |
| A successful tool batch clears a previous blocked state. | An unrelated successful read can mask a failed required check. | Completion requires evidence for each outstanding obligation. |
| A request without a reusable plan requires a planning model call, even for a simple answer. | Simple answers pay for planning and execution. | A bounded answer-only path with normal planning for actions. |
| Offline tests establish mechanics, not live instruction adherence. | Autonomous implementation quality and actual cost savings remain unmeasured. | Separate deterministic guarantees from dated behavioral evidence. |

For the integration branch's current behavior, see
[architecture](../../tech/architecture.md). Relevant implementation surfaces in the
reviewed T040 baseline are `src/agent/{loop.rs,instructions.rs,planner.rs}`,
`src/context/{mod.rs,agents.rs,budget.rs,compression.rs,skills.rs}`, and
`src/session/mod.rs`.

## Behavior Contract

### 1. Preserve human intent and clarification

- Record message origin independently of provider role. Distinguish human requests,
  human steering and question answers, runtime continuation, and tool/model output.
  Preserve the original transcript and stable message indexes.
- Keep a bounded representation of the current human goal, applicable corrections,
  decisions, and unresolved questions across compression, resume, and step changes.
  Runtime continuation text must not replace that representation.
- Preserve source references for decisions and answers. A summary is evidence, not
  fresh authorization; neither tool output nor quoted instructions may manufacture
  human provenance. Unknown legacy origin remains explicitly unknown.
- Inspect already available context before asking. Ask when an unknown changes
  correctness, scope, material cost, or an irreversible action. State reasonable
  low-risk assumptions and continue work that does not depend on a missing answer.
- Do not repeat an answered question unless changed scope or contradictory evidence
  makes the old answer insufficient. A skipped or unavailable answer remains unresolved;
  affected work must wait or reconcile as blocked, while independent work may continue.

### 2. Resolve applicable project instructions

- Resolve the repository-root-to-target instruction chain within the active worktree.
  Keep source path, scope, and content identity with each selected instruction.
  Ancestor rules remain applicable; a more specific rule wins within its scope when
  rules conflict. Explicit user direction and runtime permission boundaries retain
  their existing precedence.
- Apply instruction sources in this fixed low-to-high order: configured global rules
  (or the existing home fallback), then repository directories from root to target.
  In each directory select `AGENTS.md`, falling back to `CLAUDE.md`, as the base; then
  select `AGENTS.local.md`, falling back to `CLAUDE.local.md`, as the local override.
  Load at most one base and one local file per directory. Runtime permissions remain
  authoritative, current explicit human direction governs task intent, deeper scopes
  override ancestors only for paths beneath them, and selected skills cannot weaken
  any higher-authority rule.
- Load nested instructions before work in their directory. For work spanning scopes,
  apply each rule to its paths and surface conflicts that cannot be resolved by scope.
  A filesystem tool uses its validated target path as scope. A terminal call uses its
  working directory plus explicitly declared affected paths; a mutating opaque command
  without bounded affected paths is treated as repository-wide and fails closed when
  bounded instruction discovery cannot prove complete coverage. Command-text filename
  guessing is never proof of scope.
- Read referenced specs and project memory selectively when required for the task.
  Cache bounded reads by source identity; refresh affected instructions when the
  worktree, scope, or source changes, without rescanning the repository every turn.
- Use bounded, identity-checked reads and preserve aggregate prompt limits. If required
  instructions cannot be read or fit, report the missing context before dependent
  work. Never silently truncate required policy and claim it was followed.
- Changed project instructions or skills must not silently remove active execution
  restrictions or expand approved scope. Apply existing approval and reconciliation
  semantics when a change requires renewed authority.

### 3. Select skills economically

- Preserve explicit user selection and configured active-skill selection. Missing or
  ambiguous explicit selections receive a clear diagnostic.
- Automatic selection must use meaningful task relevance. Generic description words
  alone must not activate a skill. Use deterministic ordering, deduplication, and
  bounded selection; do not add a model call solely to rank skills.
- Discover and cache bounded frontmatter before loading bodies or references. An exact
  case-insensitive skill-name phrase is a match. Otherwise require an exact declared
  non-generic tag or at least two distinct non-stopword description tokens present in
  the task; the versioned stopword set applies to tags and descriptions, so one generic
  word never matches. Rank exact name, tag count, then
  description-token count, with canonical path as the stable tie-breaker. Select at
  most three automatic skills within the aggregate prompt budget. Explicit and
  configured selections are not displaced by that automatic limit.
- Separate cheap discovery metadata from loading selected bodies and references where
  practical. Record a bounded reason for selection and reuse unchanged selections.
- A non-selected skill contributes no prompt text, constraints, or hooks. Explicit
  selections that exceed the context limit must be surfaced, not silently dropped.
  Existing denial and approval policies continue to apply.

### 4. Require evidence for completion and recovery

- Represent required verification and unresolved execution failures in the existing
  plan/session model, linked to the exact plan, step, worktree revision or content
  identity, and audited invocation. Avoid a second workload database.
- Add a backward-compatible verification-obligation collection to each plan step.
  Each obligation has a stable ID, description, required flag, status, plan/step
  binding, affected-path scope, worktree/content identity, audited invocation ID,
  timestamp, and bounded reason. Supported statuses are `pending`, `running`, `passed`,
  `failed`, `cancelled`, `stale`, and `waived`; unknown serialized values fail closed.
  Obligations come from explicit human requirements, applicable project gates, or an
  approved plan. Model text may propose an obligation but cannot mark it passed or
  waived.
- A required check can be pending, running, passed, failed, or explicitly waived by
  authorized user scope change. Missing, cancelled, or interrupted results do not pass.
  A later unrelated successful command cannot clear a failed obligation.
- A corrective rerun resolves only the obligation it verifies. Later relevant edits
  invalidate earlier passing evidence; until dependency tracking is justified, prefer
  conservative invalidation to reusing potentially stale results.
- A successful audited invocation passes only the obligation named by its exact
  invocation binding. Any later mutating tool in the same worktree conservatively
  marks passed obligations stale unless the mutation is provably outside their bounded
  affected-path scope. Do not infer success or identity by parsing arbitrary stdout.
- Expected discovery misses, such as a search with no matches, must not require
  meaningless successful reruns. Define typed probe outcomes and a bounded recovery
  contract; a model's assertion alone cannot waive a failed required gate.
- A completion request validates the current obligations against persisted evidence.
  Unresolved obligations leave the plan incomplete with an actionable reason in both
  live and reloaded views. A waived check is reported as waived, never as passed.
- A waiver requires an explicit human scope change recorded by source message index
  and plan ID. It may remove a check that became inapplicable; it cannot relabel failed
  evidence or waive runtime safety and mandatory project gates. Legacy sessions load
  absent message origin as `unknown` and absent obligations as empty, then derive any
  currently required pending obligations before further execution. They never
  manufacture human provenance or historical passing evidence.
- Preserve cancellation, policy denials, exact plan identity, run leases, continuation
  handling, and the T040 repeated-failure guard. Resuming or delegating work must not
  silently erase outstanding obligations or reuse evidence from another plan.
- For nib self-development, verify actual source edits and test artifacts in the
  managed worktree, review the diff, and report remaining work. Source modification
  does not replace the running executable or imply merge/release authority.

### 5. Reduce planning overhead for simple answers

- Design an opt-in answer-only route for requests satisfiable from available context.
  It must have no executable tools and make at most one generation request when the
  answer succeeds. It must not add a separate model classification call.
- Configure the route as `agent.answer_only = false` by default. When enabled, it is
  eligible only for a new interactive request with no active plan or run and no caller
  requirement for planning. The one bounded request receives no executable tools and
  may either return the answer or select one non-executable `request_plan` control.
  Content completes only the answer-only activity; it never creates, advances, or
  completes a plan.
- Requests needing inspection, clarification, or actions use the normal approved-plan
  path. An ambiguous or unsupported answer-only result falls back once; it cannot
  initiate tools or invent missing evidence. Record the route and fallback outcome.
- Preserve session ownership, cancellation, audit, and honest final-state reporting.
  Answer-only activity must not complete or modify an unrelated active plan.
  Projects requiring planning for every request keep that behavior. `request_plan`
  discards partial answer content and enters the normal planner exactly once; malformed
  control output or transport failure is reported and does not loop through both routes.
  Persist bounded `started`, `completed`, and `fallback` route events for accounting.
  Measure fallback cost as well as successful savings.

## Acceptance Criteria

- [ ] Synthetic continuation cannot displace the latest applicable human correction
      under history pressure, compression, or resume; unknown legacy provenance is safe.
- [ ] Answered and unresolved questions survive step changes, with no automatic consent
      inferred from missing input and no dependent action before a required answer.
- [ ] Root and nested instruction precedence, multi-scope tasks, changed files, bounded
      reads, and unreadable required instructions have observable regression tests.
- [ ] Explicit skill selection remains supported; irrelevant generic-word matches do
      not load bodies, apply constraints, or run hooks; ordering and limits are stable.
- [ ] A failed required test followed by a successful unrelated read remains incomplete.
- [ ] A valid corrective rerun resolves its check; later relevant edits invalidate its
      evidence; expected discovery misses do not create permanent artificial blockers.
- [ ] Pending, failed, cancelled, stale, and waived checks have distinct persisted
      outcomes, including reload, delegation, and exact-plan replacement cases.
- [ ] Successful answer-only fixtures use one generation request and no executable
      tools; unsupported cases fall back once without side effects or plan corruption.
- [ ] A self-development fixture produces the intended code diff, exercises a failure
      and repair, passes required checks, and reconciles the same managed worktree.
- [ ] Resource evidence records model requests, tool attempts, context size, compression
      requests, and repeated questions for baseline and candidate on the same fixtures.
      Correctness and required verification cannot be traded for lower counts.
- [ ] Prompt guidance tests and runtime enforcement tests are identified separately;
      deterministic fixtures are never described as live-model compliance evidence.
- [ ] Compatibility, documentation, two-stage review, and canonical validation gates
      pass, with remaining live-qualification limits stated explicitly.

## Affected Areas

| Area | Expected impact |
| --- | --- |
| Agent and context modules | Intent retention, instruction resolution, skill selection, completion validation, answer routing. |
| Session and plan persistence | Backward-compatible origin and verification evidence; atomic updates under existing plan/run identity. |
| Tool execution and delegation | Typed outcome/evidence association, scope handoff, child-result validation. |
| Plain/TUI interaction | Shared projections of missing answers, unresolved checks, and completion reasons. |
| Configuration | Explicit answer-only enablement and bounded context/skill policy as selected during design. |
| Tests and Taskfile | Deterministic regression matrix and resource counters through existing Task targets. |
| Documentation | Architecture, user guide, configuration/migration behavior, and spec inventory. |

Existing session files must remain readable without manufacturing provenance or passed
checks. Before adding persisted fields, document defaults, validation, old-binary
compatibility, and a safe rollback policy. Provider-private continuation state must
remain private and bound to its existing identity.

## Implementation and Rollout Plan

1. Use integrated T040 revision `c64f953` as the baseline and capture its resource
   counts for the fixed T041 fixtures before the first production change. The design
   decisions are settled below; split child development specs only if scope expands.
2. Add message provenance and bounded human-intent retention, including legacy fixtures.
3. Add instruction scope/refresh and conservative skill relevance with bounded reads.
4. Add verification obligations and evidence-based recovery using the existing session
   authority. Qualify ordinary, resumed, and delegated execution before proceeding.
5. Implement the opt-in answer-only route after its design review; compare complete
   route costs, including fallback, against the recorded baseline.
6. Run offline self-development and clarification scenarios, two-stage review, and
   canonical gates. Update docs and record exact revision evidence before closure.

## Regression Test Plan

Every scenario below is **planned, not implemented**. Reuse existing fixture helpers
and extend the appropriate suite as each production slice lands. A scenario may need
multiple tests to keep success and failure assertions focused. Do not add ignored or
always-passing placeholders, and do not weaken an assertion to fit current behavior.

| ID | Scenario | Required observable result | Test home and Task gate |
| --- | --- | --- | --- |
| T041-01 | Human correction followed by synthetic continuation and large tool output; repeat after compression and reload. | Bounded requests retain the applicable human correction and its source; synthetic text never acquires human provenance; raw history remains intact. | `src/context/` tests via `task test:agent-context`; persistence cases in `tests/session_roundtrip.rs` via `task test:integration`. |
| T041-02 | Answer a clarification, advance a step, then resume; separately skip or cancel a required answer. | The answer and unresolved state survive. The answered fixture advances without another question; unanswered dependent work performs no action and stays incomplete. Cancellation reconciles the run. | `src/agent/` and `tests/test_runtime_e2e.rs` via `task test:agent-context` and `task test:runtime-e2e`. |
| T041-03 | Root and nested instructions conflict, a task spans directories, then an instruction changes. | Request payloads contain the correct scoped rules and refreshed identity; unaffected reads are reused; runtime restrictions retain precedence. | `src/context/` tests via `task test:agent-context`; scoped execution in `tests/test_runtime_e2e.rs` via `task test:runtime-e2e`. |
| T041-04 | Required instruction is unreadable, oversized, linked outside the allowed scope, or cannot fit the prompt budget. | Bounded reading/assembly returns an actionable missing-context result; dependent execution does not proceed as if the full instruction had been read. | `src/context/` tests via `task test:agent-context`; dependent-action guard in `tests/test_runtime_e2e.rs` via `task test:runtime-e2e`. |
| T041-05 | Explicit skill, missing explicit skill, generic-word false match, duplicate discovery, and over-budget explicit selection. | Selection order/reasons are deterministic; explicit failures are visible; unrelated skills supply no body, constraints, or hooks; ranking adds zero generation requests. | `src/context/` tests via `task test:agent-context`; hook/policy effects in `tests/test_runtime_e2e.rs` via `task test:runtime-e2e`. |
| T041-06 | Required check fails; an unrelated read succeeds; model requests completion. Then perform a valid corrective rerun. | The unrelated success leaves the original obligation failed and the plan incomplete. Only the corrective check's audited success resolves that obligation. | `tests/test_runtime_e2e.rs` via `task test:runtime-e2e`. |
| T041-07 | Pass a required check, edit relevant source, and request completion; separately cancel a running check, record an expected probe miss, and apply an authorized waiver. | Edited evidence becomes stale; cancelled/missing results never pass; an expected probe miss does not become a permanent blocker; a waiver remains distinct from a pass. | `src/agent/` tests via `task test:agent-context`; persistence in `tests/session_roundtrip.rs` via `task test:integration`. |
| T041-08 | Resume legacy state; replace a plan; supply stale or mismatched delegated evidence. | Legacy provenance stays unknown; evidence cannot cross plan/worktree identities; unresolved obligations survive reload and invalid child results. | `tests/session_roundtrip.rs` via `task test:integration`; `tests/delegation.rs` via `task test:delegation`. |
| T041-09 | Opt-in answer-only success, action request, unsupported response, active unrelated plan, and project requiring planning. | Eligible success uses one generation request and zero executable tools; fallback occurs at most once; normal approval applies to actions; unrelated plan state stays intact. | `src/agent/` and `tests/test_runtime_e2e.rs` via `task test:agent-context` and `task test:runtime-e2e`. |
| T041-10 | Implement a bounded Rust change in one managed worktree, encounter a failing behavior test, repair it, verify, and review the diff. | The intended source artifact exists, required Task gates really pass on the final content, no unrelated paths change, and the same plan/worktree reconciles from failure to completion. | Extend the existing source-edit fixture in `tests/test_runtime_e2e.rs` via `task test:runtime-e2e`; this proves mechanics, not arbitrary live self-development. |
| T041-11 | Run the same scenario inputs against T040 and the candidate, including answer-only fallback and unchanged context reuse. | Counters record actual generation/compression requests, tool attempts, context size, and repeated questions. The resolved resource policy and any stricter slice-specific ceiling are recorded before that slice lands; gains cannot hide failed assertions or skipped gates. | Reuse agent/context and runtime fixture counters via `task test:agent-context` and `task test:runtime-e2e`. |
| T041-12 | Policy denial, three identical failed batches, cancellation, and misleading tool/file text around completion. | Denied actions do not retry; the existing failure bound and terminal reconciliation hold; supplied text cannot forge approval, human origin, or passing check evidence. | Extend T040 regressions via `task test:runtime-e2e`; verify live/reloaded projections through `task test:interactive`. |

For each scenario, inspect captured requests, audited tool calls, persisted plan/check
state, and produced artifacts as applicable. Scripted model responses verify runtime
handling; they do not prove that a live model will choose the desired response. Keep
live instruction-adherence evaluation under T023. Record the implemented test names
and exact passing revision here as slices are completed.

## Validation Gates and Evidence Boundary

During implementation use `task check` and the narrowest relevant focused targets:
`task test:agent-context`, `task test:runtime-e2e`, `task test:delegation`, and
`task test:interactive`. Closure requires `task verify`, `task docs:check`, and
`git diff --check`. Add repeatable scenario runners to Task instead of ad hoc scripts.
Persistence, filesystem, or process changes also require relevant native CI evidence.

`task test:agent-context` and `task test:integration` are present on the integrated T040
baseline. The test plan above names the intended focused gates; it does not claim the
T041 scenarios are implemented. `task docs:check` validates this reconciliation.

The offline matrix must cover success, failure, correction, changed scope, cancellation,
resume, legacy state, context pressure, policy denial, and misleading file/tool text.
Use deterministic Mock or credential-free localhost fixtures. Count actual calls and
inspect produced artifacts; avoid judging success from the final model sentence.

Live behavior assessment belongs to T023's existing provider/model qualification
process. Proposed scenarios include clarification, instruction selection, failed-check
recovery, and a bounded source-edit task. Before any live run, record exact scenario
and model versions, repetitions, pass rubrics, cost/request/time ceilings, and required
authority under T023. Report all failures and usage; a few successful runs cannot prove
universal compliance. T041's offline closure does not close T023 or authorize paid runs.

## Resolved Design Decisions and Residual Risks

- **Instruction precedence and refresh:** use the fixed global/root-to-target and
  base/local precedence above. Cache by worktree identity, canonical scope, and stable
  file identity; refresh only affected entries when any identity changes. Incomplete
  bounded discovery blocks dependent work instead of silently omitting rules.
- **Evidence identity:** store obligations inside the existing plan/session authority,
  bind results to audited invocation and worktree/content identity, and invalidate
  conservatively after relevant mutation. Legacy absence means unknown or pending,
  never passed. Only an explicit human scope change can produce `waived`.
- **Skill relevance:** use frontmatter-first discovery, the exact name/tag/two-token
  threshold, a three-skill automatic limit, and stable ranking. Explicit and configured
  selections remain authoritative and fail visibly when missing or over budget.
- **Answer routing:** use the disabled-by-default single-request route and typed
  `request_plan` control above. One fallback is allowed; answer-only execution has no
  executable tool authority and cannot mutate unrelated plan state.
- **Resource regression:** capture the T040 baseline at `c64f953` with the same fixtures.
  Direct answer success is exactly one generation request, zero executable tool calls,
  zero compression requests, and no repeated question. Fallback adds at most the one
  answer-only attempt before the unchanged normal path and performs no tool call first.
  Unchanged instruction/skill inputs add no generation request and reuse cached reads.
  Other fixtures must not exceed their T040 generation or compression count unless a
  new correctness obligation requires it and the spec records a scenario-specific
  ceiling before that slice lands. Estimated tokens are labeled when provider usage is
  absent; a lower count never compensates for a failed correctness assertion.

Residual risks are larger scoped-policy prompts, conservative evidence invalidation,
false-negative automatic skill selection, and answer-route fallback overhead. The
bounded caches, visible diagnostics, explicit-skill path, persisted route/evidence
state, and fixed regression matrix mitigate them. Remaining non-goals are a new workload
database, unbounded indexing, provider-specific tokenizers, changes to continuation
compression, unattended self-improvement, automatic merge/publication, and executable
replacement.
