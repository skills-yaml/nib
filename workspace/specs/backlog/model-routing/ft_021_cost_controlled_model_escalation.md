# FT-021: Cost-Controlled Model Escalation

**Status:** Backlog — proposal requested on 2026-09-30; model choices, spending

State: backlog
Primary Feature: model-routing
limits, and quality targets require decisions before development.

## Summary

Run most nib work on a cheaper or open-weight model. Invoke a costly model only
when evidence indicates that its assistance is likely to improve the outcome.
Prefer bounded diagnosis or planning advice, then let the cheaper worker continue;
allow a bounded takeover when advice is insufficient. Optimize total cost per
verified completed task while preserving session truth and human control.

This document proposes behavior. It does not describe a shipped routing feature.

## Problem and Users

Developers and other nib users currently select a provider/model for a run.
Using a costly model for repository inspection, routine planning, focused edits,
and repeated tool observations spends premium inference on work that a cheaper
model may handle adequately. Selecting a cheap model alone can instead lead to
repeated failures, incomplete requirements, and expensive recovery.

Task-type selection is insufficient: a short request can conceal a complex
dependency, and difficulty becomes clearer after inspecting the repository or
attempting the work. The required capability is recognizing useful progress,
requesting assistance when justified, and verifying the resulting artifact.

“Rarely” must be measured. A small number of costly calls can still dominate
spending if each receives a large history or produces a long response.

## Goals and Scope

- Use the configured cheaper worker for ordinary planning and execution.
- Support open-weight workers through an explicitly configured, qualified
  transport, whether hosted locally or remotely.
- Escalate from observed work and verification evidence at safe work boundaries.
- Usually request help on a specific unresolved problem, with bounded input and
  output, before considering a full takeover.
- Bound worker attempts, expensive assistance, and aggregate task spending.
- Preserve the same verification, permission, and reconciliation requirements
  for every model.
- Expose model choices, escalation reasons, usage, and task outcomes to users.

Initial scope covers coding work with objective checks. Architecture, research,
and documentation require explicit acceptance rubrics before equivalent routing
quality can be claimed. nib remains an AI agent; coding is the first evaluation
workload, not a change in product category.

## Non-Goals

- Training or fine-tuning a router in the first delivery.
- Switching models on every tool call or performing token-level speculative decoding.
- Calling a costly planner or reviewer for every task.
- Automatically downloading model weights or provisioning inference hardware.
- Inferring live capability or pricing from model names or picker catalogs.
- Promising a savings percentage before evaluating nib's actual workload.

## Proposed Behavior

### Model Roles and User Controls

Configure explicit `worker` and `escalation` roles, each bound to a provider and
exact model ID. The worker handles routine planning, execution, and repair.
The escalation role supplies bounded advice or performs a bounded takeover.
Role names express policy; they do not assert that one model is better on every task.

Routing is initially opt-in. Users can pin a model, disable escalation, constrain
allowed providers/models, and set task limits. A pin overrides automatic routing.
With routing disabled, existing model selection behavior continues. New settings
must validate through the normal configuration and provider factory paths.

### Work and Assistance Flow

1. Admit the task with explicit roles, verification obligations, and resource limits.
2. Start ordinary planning and exploration with the worker. Continue inexpensive
   work while the worker makes useful progress within its limits.
3. At a completed-turn or plan-step boundary, evaluate audited evidence. Candidate
   triggers include repeated attempts at the same failed obligation, contradictory
   results, unresolved requirements, growing scope, or an exhausted attempt budget.
4. When justified and affordable, ask the escalation model to diagnose the specific
   blocker, resolve a design question, or revise the plan. Advice is not completion
   evidence and cannot authorize tools by itself.
5. Let the worker execute the revised approach under existing tool gates. If advice
   fails, permit a bounded takeover within the remaining task budget.
6. Verify the artifact and reconcile the exact plan, session, and delegated records.
   Budget exhaustion or unresolved obligations yield an incomplete or blocked
   outcome with preserved work, rather than successful completion.

An explicit user choice or configured, observable scope criterion may select
costly assistance at intake. Vague “high risk” classification is not sufficient
to define this rule; model selection also does not replace human clarification
or tool approval.

Initial triggers are deterministic and auditable. Exact thresholds remain open
until the evaluation policy is chosen. The worker's reported confidence alone
cannot authorize escalation or completion. Environment, transport, and permission
failures need their own recovery paths; they are not automatically evidence of
insufficient model capability.

### Context and Continuation Contract

An assistance request contains the original goal, applicable instructions,
acceptance criteria, relevant source references, current diff, observed check
results, and a precise unresolved question. Bound every section and retain the
authoritative evidence locally. Clearly identify unverified worker hypotheses.

Model changes must not reuse provider-specific continuation state bound to the
previous model. Start a fresh provider request with compatible normalized context.
Takeovers must explicitly choose between continuing the preserved artifact and
restarting a bounded attempt; neither choice may silently discard user work.
Compare these strategies in evaluation rather than assuming inherited reasoning
always helps the stronger model.

### Budget and Usage Contract

Account across planning, worker attempts, advice, takeover, verification model
calls, and linked delegated runs. Reserve allowance before dispatch, reconcile
reported usage afterward, and retain reservations across cancellation or recovery
when actual billed usage is unknown. Parallel children share the task budget.

Bound costly calls, total attempts, context, output tokens, and elapsed time.
Use reviewed pricing metadata where available, including cached input and other
billed token categories. Estimated cost must be labeled as estimated; missing
usage must not appear as zero spend. A configured hard monetary ceiling requires
sufficient pricing and enforceable request bounds; otherwise reject that budget
mode and require explicit token/request limits. Provider-side spending controls
remain necessary for charges the client cannot observe precisely.

Count local serving cost separately from API spend where applicable. Open weights
do not imply free inference or adequate tool-use performance.

### Audit and Verification Contract

Persist bounded, redacted role/model identity, routing reason, affected task/step,
attempt identity, usage availability, budget reservation/outcome, and assistance
result. Resume must preserve consumed allowances and not replay assistance
silently. Routing history must remain linked to the authoritative session plan.

Plain and TUI presentations expose equivalent routing status and controls.
Existing gateways and MCP observers receive additive, provider-neutral evidence.
Provider-private continuation data, credentials, and raw private error messages
remain outside durable records.

Keep verification tied to the exact artifact and revision. Passing tests is
evidence of covered behavior, not proof of every requirement. Check plan
obligations and review the diff; use acceptance rubrics for work without objective
tests. A model swap cannot loosen permission, sandbox, approval, or merge gates.

## Current Integration Points and Affected Areas

- `src/config/`, `src/llm/`: role resolution, explicit transports, usage and
  pricing metadata, request ceilings, and validated model changes.
- `src/agent/`, `src/context/`: progress evidence, escalation decisions, bounded
  assistance context, and revised-plan reconciliation.
- `src/tools/registry.rs`, `src/tools/delegation/`: configured role selection for
  child runs and shared task allowances. The current subagent schema exposes a
  prompt and step limit, without a model/role argument.
- `src/session/`, `src/daemons/`: durable routing evidence, accounting, and recovery.
- `src/interactive/`, `src/chat/`, `src/tui/`, `src/integrations/`: controls,
  consistent status, and observer compatibility.
- Tests, user documentation, and architecture references as implementation lands.

Related contracts: [FT-015 delegation](../../done/delegation/ft_015_subagent_delegation.md),
[T021 transport compatibility](../../done/llm-providers/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md),
[T022 provider contract](../../done/llm-providers/T022_provider_neutral_llm_contract_and_adapter_conformance.md),
[T041 verified completion](../../done/agent-runtime/T041_task_aware_context_and_verified_completion.md),
[T042 context budgets](../../done/context-memory/T042_context_budgeting_and_live_visibility.md), and
[T023 live qualification](../../development/llm-providers/T023_live_llm_provider_model_integration_qualification.md).

## Evidence and Alternatives

[FrugalGPT](https://arxiv.org/abs/2305.05176) demonstrates cost-quality benefits
from learned cascades on its evaluated workloads. Those results do not establish
a savings target for nib.

[Strong–weak collaboration for repository-level code generation](https://arxiv.org/abs/2505.20182)
reports equivalent strong-model performance at about 60% generation cost for
one configuration. Cheap-first approaches suit tight budgets, while strong-first
approaches can perform better at larger budgets. Its Agentless Lite experiments
cover 300 Python repository issues and exclude retrieval cost, latency, and energy.

[SWE-Router](https://arxiv.org/abs/2607.00053), a 2026 workshop paper, routes after
short cheap-model exploration using partial-trajectory evidence. It supports
evaluating progress-aware routing; its trained value head and benchmark results
do not validate the deterministic rules proposed here. Its expensive agent
restarts from the original request, so advice followed by worker continuation
is a design hypothesis requiring separate evaluation.

An always-costly baseline is straightforward but conflicts with the desired
operating cost. An always-cheap baseline measures whether escalation adds value.
A costly planner on every task may help difficult work but cannot satisfy rare
costly invocation. A prompt-only classifier adds inference and can miss hidden
complexity. Repeated cheap sampling requires a reliable selector and may spend
the savings on unsuccessful attempts. A learned router is a later option once
nib has representative labeled outcomes.

## Delivery and Evaluation

1. Define worker hosting, role/model candidates, ceilings, workload, and acceptance
   targets. Build baseline evidence under the same harness and task requirements.
2. Deliver explicit role selection and complete task accounting with routing off.
3. Add opt-in deterministic escalation and bounded advice, then qualified takeover.
4. Compare mixed routing against always-cheap and always-costly configurations on
   held-out tasks. Expand only when quality and spending targets pass.
5. Consider learned routing after enough representative failure and success data exists.

Measure verified completion rate, requirement misses, regressions, human repair,
total spend per verified completion, latency, attempts, escalation frequency, and
costly token share. Include failed tasks and router/review overhead in aggregate
spend. Define “rarely” using both the fraction of tasks invoking the costly model
and costly call/token usage; select thresholds before running the comparison.
Report uncertainty and task categories rather than relying on one aggregate score.

## Acceptance Criteria

- [ ] Routing-disabled and explicitly pinned runs preserve existing selection behavior.
- [ ] Ordinary tasks use the worker for planning and execution without a compulsory
  costly planner, classifier, or reviewer.
- [ ] Every costly invocation has an auditable trigger, bounded context, and reserved
  allowance; advice and takeover are distinguishable outcomes.
- [ ] Worker continuation after advice retains applicable instructions, plan identity,
  and verification obligations; unsupported continuation reuse is rejected.
- [ ] Repeated failure, budget exhaustion, cancellation, and restart cannot exceed
  configured dispatch limits or silently reset shared delegated allowances.
- [ ] Missing pricing or usage produces explicit unknown/estimated evidence, never
  a false zero-cost or hard-ceiling guarantee.
- [ ] Model changes cannot bypass tool permissions, isolation, or verified merge gates.
- [ ] Plain/TUI controls and persisted evidence agree about role, reason, and outcome.
- [ ] A held-out comparison meets the predeclared savings, costly-use, quality, and
  latency targets; unsupported workload categories remain explicitly unqualified.

## Validation Gates

For this proposal: `task docs:check` and the repository-required `task verify`.
For implementation: focused deterministic runtime, delegation, context, interaction,
and provider-conformance tests, followed by `task verify` and `task docs:check`.
Cover successful cheap completion, advice and continuation, takeover, repeated
failure, false progress, unknown cost, concurrency, cancellation, and recovery.
Use independent artifact checks for quality evaluation. Spec compliance review
precedes code-quality review before moving implementation to done.

Paid or credentialed experiments follow T023's existing authorization and privacy
requirements. Creating this proposal does not authorize live inference spending.

## Compatibility, Risks, and Rollback

Use optional configuration and additive, versioned audit fields; existing sessions
must remain readable. Define recovery for partially recorded budget transitions
before development. Disabling routing restores explicitly selected model behavior
while retaining prior evidence and consumed allowances for an active task.

Primary risks are weak-model mistakes escaping verification, excessive retries,
context loss or inherited mistakes during handoff, stale pricing, and unobserved
provider charges. Representative qualification, artifact-bound verification,
bounded dispatch, explicit unknown accounting, and fresh compatible context address
these risks. Sending local-worker context to a remote escalation provider must
respect the configured data-sharing boundary; cost preferences do not authorize
new external destinations.

## Open Decisions Before Development

1. Will the worker run locally, through a low-cost API, or support both initially?
   What hardware, transport, and tool-use qualification applies?
2. Which exact worker/escalation models and providers may be used, with what
   pricing, data-sharing policy, and spending authority?
3. What costly invocation and token-share targets define “rarely”?
4. What savings target, quality tolerance, latency ceiling, and human repair burden
   are acceptable compared with the costly baseline?
5. Which progress signals, attempt bounds, and intake criteria trigger assistance?
6. Which task set and independent verification criteria represent nib's workload?
7. Which budget units can be enforced for each provider, including missing usage
   and local serving cost?

Resolve these decisions and record a scoped implementation plan and validation
policy before the next allowed transition, `backlog -> development`.

## Prior Memory Notes

Pending. The user preference for mostly cheaper/open-weight execution with rare
costly assistance is captured in this proposal. Hosting, models, thresholds, and
quality tradeoffs remain unsettled; do not record them as adopted decisions.
Reconcile stable decisions into `workspace/agents/memory/` when the development contract
is approved, without storing credentials or transient task traces.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | none | none | Backlog proposal only; no shipped artifact changes until separately scoped development and reservation. |

## Memory Impact

Status: none
Rationale: The proposal records open choices without adopting new policy; approved durable decisions will be classified during development.
