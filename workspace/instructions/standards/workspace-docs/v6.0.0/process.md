# Workspace Process & Governance (`workspace-docs@6.0.0`)

This document defines the operating contract for repositories adopting
`workspace-docs@6.0.0`.

## Operational Areas

- Root policy and design tokens remain in `AGENTS.md` and `DESIGN.md`.
- Static instructions live under `workspace/instructions/`.
- Lifecycle specs live under `workspace/specs/` and are grouped by primary
  feature in `backlog`, `development`, `test`, or `done`.
- Human documentation lives under `workspace/docs/`.
- Concurrent-agent WIP records live under `workspace/docs/work/multi-agent/`.
- Durable memory lives under `workspace/agents/memory/`.

## Task and Release Lifecycle

1. Register accepted work in backlog when it is not active.
2. Before non-trivial implementation, move or create the spec in development,
   initialize memory impact to `pending`, and synchronize the root catalog.
3. Implement coherent slices with affected focused gates. Format and regenerate
   artifacts before checks, review the complete behavior, stabilize the
   candidate, then run its full required gates. A branch is optional and
   requires direct user direction; it is not evidence of lifecycle progress.
4. Move the spec to test only after confirmed integration into the configured
   shared test target.
5. Move the spec to done only after confirmed production release and memory
   reconciliation.

## Concurrent Mutation

Before two or more agents mutate concurrently, assign one explicit base
revision, one dedicated linked worktree per mutating agent, and bounded, potentially overlapping
repository-relative scopes. Worktrees are detached by default. Each agent
creates its own WIP record before editing code or generated artifacts and
updates it at meaningful implementation, validation, blocker, and handoff
checkpoints.

Keep the primary checkout coordination-only. Resolve shared-file conflicts at integration; stop on unsafe worktree state
or missing branch authorization. Do not clean up a worktree
until all unique commits and uncommitted changes are safely captured.

## Completion Boundary

Local implementation can be complete while the spec remains in development.
Coordination records describe operational progress only; they do not prove
review, integration, release, approval, or durable-memory classification.

## Version Planning

Before implementation, classify each spec using the
[version reservation contract](./versioning.md). Register one shared owner and
target per component in `workspace/releases.json`, then apply the bump at the
selected development-start or merge boundary. Reconcile an occupied target
before proceeding.

## Peer Runtime

No coordinator agent is required. Every peer self-claims a unique task and
creates its own worktree through the coordination skill's atomic JSON board in
the Git common directory.
All peers may edit the same files. Independent review, serialized integration,
and explicitly configured native gates reconcile combined changes. Claims survive crashes;
failed staging worktrees remain recoverable. The runtime supports one machine
and Git common directory; it does not publish to test or production branches.

## Validation Evidence

Adopters may declare focused selectors and bounded receipts for iterative
feedback. Unknown scope falls back to aggregate validation. A final candidate
requires complete passing evidence; reuse is valid only when deterministic
native checks prove source, tests, locks, generated artifacts, gate definitions,
tool versions, and the non-sensitive environment identity remain relevantly
unchanged. Protected integration checks remain authoritative.
