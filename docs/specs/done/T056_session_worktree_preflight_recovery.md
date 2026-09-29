# T056: Session Worktree Preflight Recovery

**Status:** Done

**Related:** [FT-015](../done/ft_015_subagent_delegation.md),
[T054](../done/T054_reconcile_local_preflight_failures.md).

## Problem

In this checkout, a fresh offline Mock run stops with `worktree_preparation_failed`
before any proposed tool executes, while `nib doctor` reports healthy.
T054 intentionally keeps raw preparation errors out of the session and user
content. Session worktree reuse also treats a stale branch receipt as grounds
for recursive cleanup, which can discard uncommitted files after a legitimate
branch update.

The reproduced preflight error is `managed worktree common Git directory identity
changed`. Existing receipts retain device 2049 and inode 11711604 for `.git`;
the current directory has device 2065 and the same inode. The admission scan
reopens every completed tombstone's old Git identity even when no compaction
is needed. That makes unrelated old receipts block all new sessions.

## Scope and Goals

- Identify and correct the concrete local worktree creation blocker without
  deleting unrelated user worktrees or Git state.
- Keep T054's safe failure outcome and no-tool-execution gate.
- Keep session worktrees and uncommitted files intact when branch ownership
  cannot be safely reconciled.

## Non-Goals

- Reworking subagent merge semantics or changing the general worktree layout.
- Automatically pruning arbitrary Git worktrees or unowned registrations.

## Affected Areas

Session worktree creation and reuse in `src/integrations/worktree.rs`, managed
worktree ownership compaction in `src/sandbox/worktree/`, focused tests, and
user-facing recovery documentation. No persisted schema migration is planned.

## Design

Read and validate every durable record during admission. Recover stale Git ref
artifacts for completed records whose Git directory identity still matches.
If that identity changed, defer recovery until the record is selected for
compaction. This keeps the capacity and exact-deletion checks in force. On
session reuse, an invalid
cached ownership receipt returns an error and preserves the path, registration,
and branch for inspection. T054's generic failure outcome remains unchanged.

## Acceptance Criteria

- [x] An offline Mock mutation run creates and reuses a session worktree in the
  affected checkout or an equivalent deterministic fixture.
- [x] An unrelated completed receipt whose Git directory device identity has
  changed does not block new worktree admission while below namespace limits;
  actual compaction still fails closed on that receipt.
- [x] A changed session branch cannot trigger automatic deletion of the owned
  worktree or its uncommitted contents during reuse.
- [x] Exact-ownership safeguards for substituted paths, registrations, and refs
  remain intact.
- [x] `task check`, focused worktree/runtime tests, `task docs:check`, and
  `task verify` pass.

## Implementation Plan and Risks

First isolate the observed preflight failure. Add a deterministic regression,
then fix the smallest cause and the unsafe reuse path. Review cleanup and audit
effects before running the full gate. Fail closed on ambiguous ownership; keep
existing artifacts for inspection. Documentation will describe any operator
action needed for previously damaged records.

## Validation Gates

- `task check` and focused worktree/runtime tests during implementation.
- `task docs:check` for the spec and recovery documentation.
- `task verify` before completion.

## Memory Impact

The stable rule that invalid session receipts preserve the worktree, and the
completed-record admission rule, belong in `agents/memory/decisions.md` after
final verification. No transient device IDs or session paths belong in memory.

## Validation Evidence

The affected checkout reproduced `worktree_preparation_failed` with the
installed binary. The final optimized build of this fix created an owned
session worktree for an offline Mock run; that run later stopped at its
independent verification requirement. `task test:worktree` passed 56 sandbox
and 12 session tests, including deterministic admission and reuse regressions.
`task check`, `task docs:check`, and the final full `task verify` passed.
Earlier full runs hit timing failures in unrelated installer and delegation
integration fixtures; their focused task targets passed, and the final full
run passed both targets.
