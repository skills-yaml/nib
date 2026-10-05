# Migration Notes: workspace-docs@6.0.0

These notes define the breaking workflow change from `workspace-docs@5.0.0`
to `workspace-docs@6.0.0`. Follow the canonical
[Agent Migration Guide](../AGENT_MIGRATION.md) first.

## Breaking Change

Version 6 requires dedicated linked worktrees and repository WIP records when
two or more agents may mutate concurrently. Worktrees are detached by default;
creating or selecting a task or delivery branch requires a direct user request
for the named task.

## Updating from 5.0

1. Create or move the migration spec to development and update the root
   catalog without changing lifecycle state based only on branch names.
2. Install the complete 6.0.0 package from a trusted source.
3. Preserve manual `AGENTS.md` content and replace only its generated context
   with `agents-template.md`.
4. Create `workspace/docs/work/multi-agent/` and retain existing work docs.
5. Adopt the coordination record schema and add the repository-native
   coordination validation gate.
6. Update mutating skills and profiles to use detached dedicated worktrees,
   WIP records, scope ownership, recovery, and safe cleanup.
7. Preserve existing branches and worktrees. Do not attach, move, or delete
   them merely to complete the migration.
8. Run the audit checklist and all repository Taskfile gates.
9. Add a conservative gate map before enabling focused-result reuse. Unknown
   scope and missing receipts must retain aggregate validation.

## Active Coordination During Migration

Inventory existing concurrent assignments before enforcing the new contract.
Create records for still-active work without inventing history, assign one
owner per overlapping path, and preserve every dirty worktree or unique commit
until safely captured. Read-only agents remain exempt.

## Rollback

Before release, return to the 5.0.0 pin and revert only files changed by this
migration. Preserve operational records and worktrees containing unique work.
After release, use a reviewed follow-up version; never rewrite released 6.0.0
artifacts.

## Spec Versioning Migration

The 6.0 candidate also requires [spec version impact](./versioning.md).
Add the Version Impact table to every current spec, preserving legacy files.
Initialize `workspace/releases.json` from actual released baselines and current
candidates, recording already-applied versions rather than bumping again.
Freeze historical done paths in `historical_specs`; do not infer old release
numbers. Assign one owner per open component release, document the bump timing,
and add `task versions:check` to the aggregate gate before implementation.

## Coordinator-Free Peer Migration

Install the portable coordination runtime and invoke its bundled CLI directly;
a Taskfile wrapper is optional. Retain existing WIP records and worktrees.
Initialize a fresh atomic JSON board under the Git common directory from
a reviewed committed base only after existing concurrent writers are quiesced.
Peers then self-claim unique tasks and create their own detached worktrees; a
coordinator role is optional. Shared file scopes are always allowed. Reconcile
conflicts through peer review and serialized integration. Do not infer runtime
claims from stale WIP copies or adopt existing dirty worktrees automatically.
Do not copy or publish `board.json`, its lock, managed worktrees, or recovery
archives. Preserve them in place while work remains active.
