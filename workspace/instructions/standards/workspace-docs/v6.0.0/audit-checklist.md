# Audit Checklist (`workspace-docs@6.0.0`)

## Structure and Lifecycle

- Required root files and Workspace directories exist.
- Every current spec has one state, primary feature, and catalog row.
- Test and done states have integration and release evidence respectively.
- Memory impact follows the selected lifecycle state.

## Multi-Agent Coordination

- Concurrent mutating agents use exclusive linked worktrees from one explicit
  base revision.
- The primary checkout is coordination-only while concurrent mutation is
  active.
- Worktrees are detached unless the record contains dated direct-user branch
  authorization.
- Every agent record uses safe identifiers, repository-relative bounded
  scope, the required status schema, and all handoff sections.
- Records contain no secrets, absolute local paths, host data, or raw command
  transcripts.
- Interrupted work remains recoverable and cleanup cannot discard unique
  uncommitted or unreferenced work.

## Validation

- Run `task coordination:check` after assignment or status changes.
- Run `task check`, `task test`, and project-specific gates.
- Confirm focused checks preceded final review where applicable and that final
  gate evidence identifies the unchanged candidate.
- Reject stale receipts, unexplained broad repeats, unsafe test concurrency,
  and local evidence presented as proof of a different integration revision.
- Verify the generated `AGENTS.md` block matches the pinned package.
- Verify released older package directories remain unchanged.

## Spec Versions

- Every non-legacy spec has a valid per-component Version Impact table.
- Reservations declare baseline, target, impact, timing, owner, members, and evidence.
- Concurrent agents check occupancy through a shared transactional reservation board.
- Already-applied shared candidates are reused without duplicate bumps.
- `task versions:check` rejects collisions, stale membership, invalid bump
  arithmetic, source drift, and versions not applied at the selected boundary.
- Historical exemptions list only pre-migration done specs; no release history
  is fabricated.

## Peer Environment

- Peers initialize and join without a required coordinator agent.
- Atomic task claims coexist with always-shared file scopes.
- Review binds to the exact handoff commit and excludes self-review.
- Internal integration advances only after merge and native gates succeed.
- The local peer board is untracked atomic JSON under the Git common directory;
  it is not copied into delivery branches or durable memory.
- Interrupted work, conflict stages, and uncaptured commits remain recoverable.
- Runtime state stays local to one Git common directory and is never published.
