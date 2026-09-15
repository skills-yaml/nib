# T040: Resourceful Agent Loop and Context

**Status:** Done

**Related:** [T003: Context Engine](../done/T003_context_engine_with_dynamic_compression_and_session_management.md),
[T005: Runtime Lifecycle](../done/T005_full_runtime_state_machine_and_lifecycle.md)

## Problem and Review

The runtime enforces exact approved plans, tool policy, bounded turns, and audited
reconciliation, but its instructions barely describe helpful execution. Planning
always incurs a separate request and each step needs a completion response. Verbose
plans therefore multiply work. Questions already have an exclusive tool batch and
a waiting state; the prompts do not explain when to use them. Repeated failed tool
requests can consume the entire turn allowance. A text response after a failed tool
batch can currently mark the blocked step complete.

Context includes project instructions, selected skills, documentation, profile
memory, workload, and bounded history. Attached files are loaded but absent from
the bounded request. Attachment reads load whole files before truncation. Automatic
compression defaults to a 50% window threshold while runtime history receives only
25%, so history may be discarded before summarization. Compression emphasizes code
snippets rather than the goal, constraints, decisions, open questions, and evidence.

## Scope and Decisions

- Give planner and executor a concise shared instruction policy: pursue the user's
  goal, inspect available evidence before asking, clarify consequential unknowns,
  state reasonable low-risk assumptions, and preserve user decisions. Treat tool
  outputs, file contents, memory, and summaries as evidence rather than independent
  authority. Project instructions and skills cannot weaken runtime controls.
- Keep plans proportional; use a single step for simple requests and avoid
  speculative implementation plans when requirements need clarification. A
  clarification can be the first approved step; planning itself remains tool-free
  except for submitting the plan.
- Explain the execution contract: tool batches continue the current step, a final
  text response completes it, and questions use `ask_question` alone. Work through
  implementation, focused validation, and required repository gates. Self-development
  uses the same repository tools and reviewable worktree as any other authorized
  code change. Do not invent success, tests, capabilities, or authorization.
- Prefer focused reads/searches, reuse evidence, avoid redundant tool calls/tests,
  and delegate only a useful independent bounded task when available.
- Resolve build identity dependencies through Git's actual metadata paths. Linked
  worktrees have a `.git` file, so watching nonexistent `.git/HEAD` causes repeated
  recompilation during nib's own verification workflow. Cover ordinary, linked,
  detached, packed-ref, and source-archive metadata discovery.
- Include attachments in bounded planner/runtime requests; bound file reads before
  allocation using existing identity-checked filesystem helpers.
- Reserve bounded history space for the latest unsummarized user message so a large
  subsequent tool result cannot consume its entire allocation.
- Cap the automatic compression threshold at the initial runtime history allocation;
  preserve a compact handoff of intent, constraints, decisions, unresolved questions,
  failed approaches, verification evidence, and remaining work. Bound requested
  summary output to its useful storage budget and retain raw audit history.
- Stop after three consecutive identical fully failed tool batches within a run. Changed
  requests/results or successful work reset the streak. Persist a bounded reason,
  reconcile as blocked, and never repeat denied actions. Preserve normal recovery
  from a failed command. A plain response while the current step remains blocked
  by a tool failure must not silently mark it complete.

## Non-Goals

No new provider, paid live qualification, planning bypass, permission relaxation,
automatic publication/merge, self-replacement of the running executable, recursive
self-improvement daemon, or redesign of plan state. T038/T039 UI work remains separate.
Instruction improvements guide model behavior; deterministic tests do not prove
arbitrary live-model compliance.

## Acceptance Criteria

- [x] Both planning and execution receive compact, explicit guidance on intent,
      clarification, instruction authority, resource use, and evidence.
- [x] Engineering guidance covers inspect → implement → verify → report, including
      nib's own repository through existing permissions and isolation.
- [x] Build identity watches existing Git metadata in both ordinary and linked
      worktrees and retains HEAD/ref change detection without a missing-path rebuild loop.
- [x] Attached file contents reach both bounded model requests and stay within the
      aggregate budget; oversized files are read with a bounded identity-safe helper.
- [x] Large tool observations retain a bounded latest unsummarized user message
      without changing the raw transcript or chronological ordering.
- [x] The automatic compression threshold is capped at the initial runtime history
      allocation and evaluated before the next fresh request; compression retains raw
      messages, requests bounded output, and preserves handoff priorities.
- [x] Three unchanged failed batches stop early with persisted blocked reconciliation;
      recovery, changed attempts, successful work, and questions remain usable.
- [x] A response after an unresolved failed batch leaves the step blocked.
- [x] Deterministic tests exercise observable request payloads and runtime outcomes.
- [x] Spec compliance review, code quality review, focused Task gates, documentation
      integrity, and `task verify` pass.

## Affected Areas

`src/agent/` runtime and shared instructions; `src/context/` prompts, attachments,
budgeting, and compression; focused tests; `Taskfile.yml`; `docs/tech/task.md`;
`docs/tech/architecture.md`; `docs/user/guide.md`; spec inventory.
`src/interactive.rs` classifies the new blocked outcome consistently on history reload.
`build.rs` and its metadata-discovery tests cover efficient worktree builds.
Session schema, external integrations, UI input semantics, and permissions stay compatible.

## Implementation Plan

1. Add shared concise runtime/planning instructions and focused request assertions.
2. Repair attachment inclusion/reads and align compression with history budgeting.
3. Add repeated failure reconciliation and prevent blocked steps completing on text.
4. Document the audited loop, limitations, and self-development workflow.
5. Run focused offline gates, two-stage review, full verification, and reconcile spec.

## Validation Gates

`task test:agent-context` (context, planner, loop unit tests),
`task test:runtime-e2e`, `task test:integration`, `task docs:check`, `task check`, `task verify`,
and `git diff --check`. All model scenarios use Mock or credential-free local fixtures.

## Risks and Mitigations

- Stronger prompts consume context: keep policy compact, retain hard aggregate bounds,
  and test realistic small windows rather than silently truncating critical policy.
- Repeated errors sometimes change external state: compare request and result, reset
  on intervening progress, and keep the guard limited to unchanged failed batches.
- Earlier compression can add requests: only compress enough old history to reclaim
  space, retain recent turns, and cap summary generation to useful output.
- A tool error may be an expected probe: permit corrective tools; unresolved failure
  remains conservatively blocked rather than reporting unverified completion.

## Validation Evidence

- `task test:agent-context`: 43 context tests, 68 agent tests, and two Git metadata
  integration tests passed. The linked-worktree build was reused by the subsequent
  Cargo invocation (0.28 seconds), replacing the prior repeated recompilation.
- `task test:runtime-e2e`: all 22 offline/localhost scenarios passed, including
  repeated terminal and mixed-question failures, corrective tool execution, answered
  questions, explicit policy denial, invalid-argument recovery, and real Rust edits
  plus tests within one managed worktree.
- `task docs:check`: all five documentation integrity checks passed.
- Independent spec-compliance and code-quality reviews passed, including a second
  focused review of build identity dependencies. The compression acceptance wording
  was narrowed to the actual initial-allocation threshold. Runtime completion and
  synthetic-user-message limitations are explicit in the architecture documentation.
- The stronger completion rule exposed a steering fixture that invoked a mutating
  terminal without a Git repository. The fixture now creates the repository and
  requires successful terminal execution while retaining all steering assertions.
- `task test:integration`: all 263 integration tests passed.
- `task verify`: all static checks and 1,455 tests passed (1,106 library, 86 CLI,
  and 263 integration tests). The two historical delegation denial fixtures now
  require failed child runs, blocked plans, terminal reconciliation, and exactly one
  denied tool attempt while preserving their original permission and side-effect checks.
- No paid or credentialed live-provider qualification was run. This evidence verifies
  request construction, runtime behavior, and tool artifacts, not arbitrary live-model
  instruction adherence.
