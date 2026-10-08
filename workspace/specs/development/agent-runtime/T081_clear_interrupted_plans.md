# T081: Clear Interrupted Plans and Explain Verification Rejections

**Status:** Development. Implementation started on 2026-10-08 at the user's request.
State: development
Primary Feature: agent-runtime

## Problem and Authority

In session `21914944` (2026-10-06), the plan "review repo" ended with its only
step `Blocked`, outcome `tool_execution_failed`. Every later chat message
("review repo", "continue", "plan", "cintinue current plan") made one
answer-route model call, received `request_plan`, and was then rejected with
`planning_required_active_plan` (`src/agent/loop/inner.rs:189-198` →
`finish.rs:455`). The rejection fires before goal matching
(`inner.rs:205`). Its message never names the plan or `/continue`, so the only
escape was `/new`.

The run ended because the model cited an undeclared `verification_id`. nib
recorded the reason only in a session event. The tool error returned to the
model was the opaque `verification binding was rejected before execution`
(`src/agent/loop/inner_exec.rs:686-697`), so the model could not correct
itself.

User decision (2026-10-07): when the interaction is interrupted, the plan is
cleared, and the user can ask for a new plan through the chat.

## Scope and Proposed Behavior

1. **Clear on interruption.** When a run terminates (reconciliation
   `continue: false`, user stop/cancel, provider or tool failure, approval
   denial) while its plan is incomplete, reconciliation removes the plan and
   appends a `plan_invalidated` event with reason `interrupted`, the previous
   plan id and goal, the step index and the terminal outcome. The step history
   stays in the session record for audit.
2. **Next chat message plans fresh.** Because no incomplete plan survives a
   terminated run, the `planning_required_active_plan` gate no longer traps the
   user. Any chat request, including "continue <goal>", routes normally and may
   create a new plan.
3. **Still-open plans are untouched.** A run that is waiting on the user (a
   pending approval card or question form, T061/T067) is not interrupted, and
   its plan stays active. If the gate still fires in that state, its message
   names the plan and points to the pending prompt or `/new`.
4. **Explain verification rejections.** The tool error returned to the model
   includes the rejection reason and the step's declared verification ids, for
   example `verification "review-task-check" is not declared on step 0;
   declared: [...]`.
5. `/continue` on a cleared plan explains that it was cleared and why. The
   planning-gate message names the remaining ways forward: the pending
   question, `/continue <plan-id>` or `/new`. A generic "plan was cleared"
   note on every stop line was dropped in review, because it would also
   appear when no plan was cleared.

## Exclusions and Compatibility

- `/continue <plan-id>` remains for plans that are still incomplete but not
  terminated (for example, provider-continuation recovery that keeps the plan).
  It reports "plan was cleared" for an interrupted plan.
- Existing tests that resume a terminated, incomplete plan must be updated to
  the new contract. These are listed during planning.
- No change to plan approval, verification semantics or workload persistence
  beyond clearing.

## Affected Areas

- `src/agent/loop/finish.rs`: terminal reconciliation and the
  `planning_required_active_plan` message.
- `src/agent/loop/inner.rs` (gate), `src/agent/loop/verify.rs`
  (`invalidate_plan_in_session` reason) and `src/agent/loop/inner_exec.rs`
  (verification error text).
- `src/interactive/split_00.rs:1405` and the `/continue` handler at
  `src/interactive/split_02.rs:1267`.
- Session and interactive tests, the user guide, the catalog and memory.

## Acceptance Criteria

- [ ] AC-1: With the mock provider, a run that ends with a `Blocked` step clears
  the plan and records `plan_invalidated` with reason `interrupted`. The next
  chat message ("continue", the same goal or a new goal) is not rejected with
  `planning_required_active_plan`.
- [ ] AC-2: User stop/cancel during a step clears the plan in the same way.
- [ ] AC-3: A run paused on an approval card or question form keeps its plan,
  and resuming through that prompt behaves as before.
- [ ] AC-4: An undeclared `verification_id` returns a tool error that contains
  the reason and the declared ids, and the model can retry within the same run.
- [ ] AC-5: `/continue <id>` on a cleared plan returns a clear "plan was
  cleared" error.
- [ ] AC-6: `task verify` passes. Guide, catalog, versions and memory are
  reconciled. A review is required because this changes the workload model.

## Implementation Plan

1. Classify terminal outcomes that clear an unfinished plan
   (`clears_unfinished_plan`): agent failures, cancellation, `local_error` and
   unresponsive shutdown. Keep plans for `unresolved_clarification`,
   `provider_continuation_interrupted` and the planning gates.
2. Clear the plan at the single run exit (`entry.rs`) for admitted runs, and
   clear plans recorded as interrupted before T081 at the next run start.
3. Explain `/continue` on a cleared plan, update the gate and stop messages, and
   list declared verification ids in rejection errors.
4. Update tests that expected interrupted plans to persist; add fixtures for
   the outcome matrix, the legacy blocked-plan session, `/continue` and
   verification errors.

## Review Revision (2026-10-08)

Independent review of `86a9cb2` required the following changes, now
implemented:
- **H1.** Clear only the plan this run bound. Each run records
  `run_plan_bound` (run id, plan id) when it routes to or generates a plan,
  and the exit clear requires that binding. A plan that waits on the user
  (any unresolved clarification or question form, including one whose run
  was cancelled) is never cleared. `plan_binding_changed` never clears.
- **H2.** Explicit compaction never clears plans, eagerly or lazily.
- **Legacy migration.** The start-of-run clear applies only to sessions with
  no `run_plan_bound` events (created before T081), and skips plans that wait
  on the user.
- **L3.** A failure to clear is logged and audited (`plan_clear_failed`) and
  never discards the run's result.
- **M1 and M2.** A mock answer-route fixture drives the real planning gate.
  Tests now prove that the gate fires for an open plan and not for an
  interrupted one, that a waiting plan survives a cancelled side request and
  a gated request, that only the bound run can clear its plan, and that the
  model receives the declared verification ids end to end.
- **Pre-existing, out of scope.** Cancelling any run still marks the open
  plan's current step `Cancelled`. That marking no longer leads to clearing
  a plan the run did not bind.

## Implementation Evidence

- Single exit: `run_agent_loop_with_runtime_and_recovery` clears after
  `runtime_terminal_event` (admitted runs only), recording
  `plan_invalidated` with `reason: interrupted`, `outcome`, `run_id`, the
  previous plan id and goal, `approved`, the step index and count, and
  `plan_outcome`.
- Legacy sessions: plans whose `plan.outcome` or blocked step outcome is an
  interruption are cleared at the next `run_started`. This is the exact shape
  of session `21914944` (`tool_execution_failed`, step 0 `Blocked`).
- AC-4: a verification rejection is an ordinary failed tool result
  (`approval_source: verification`). Only approval denials or repeated
  identical failures end the run, so the model can retry with a declared id
  from the new error text.

## Validation Gates

Focused agent-loop and interactive tests, then `task check`, `task verify`,
`task docs:check` and `task versions:check`. An independent review is required
because this changes the authoritative workload model.

## Risks and Rollback

Clearing discards in-progress plan state, so the audit event must capture
enough context to reconstruct what was abandoned. Long multi-step plans
interrupted by a transient failure must be re-planned by the user. This is
accepted by the user decision. Rollback restores the prior gate.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-catalog-refresh | Compatible interrupted-plan lifecycle change; reuses the already-applied shared 0.4.0 target (aggregate minor from T080); no second bump. |

## Memory Impact

Status: updated
Rationale: Appended the user's interrupted-plan decision to [workspace/agents/memory/decisions.md](../../../agents/memory/decisions.md) and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md) on 2026-10-08.
