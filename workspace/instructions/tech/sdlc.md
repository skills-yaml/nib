# nib Software Development Lifecycle

nib follows [Workspace Docs 7 SDLC](../standards/workspace-docs/v7.0.0/sdlc.md)
and [process](../standards/workspace-docs/v7.0.0/process.md). These are the
complete review, workflow authority, modular verification and completion contracts.

## Repository Topology and Lifecycle

Current specs live at `workspace/specs/<state>/<primary-feature>/<spec>.md`.
The [root catalog](../../specs/README.md) records exactly one feature, state and
rationale for each spec. The normal route is backlog -> development -> test -> done.
`development` is this repository's shared test branch; `main` is its completion
and production branch. This is the repository-local equivalent of the standard's
`develop` default. Local verification stays in development. Test needs confirmed
shared integration; done needs verified main merge and reconciled acceptance,
verification, docs, catalog, versions and memory. Publication is a separate event.
No direct-to-main exception is introduced by this upgrade.

Blocked work records Previous State, Block Kind (`impediment` or `deferred`),
Block Reason and Resume Condition; resume at the previous stage and renew stale
review and verification. Preserve historical done states and their older contracts.
Later fixes create linked follow-up specs rather than reopening done history.

## Project and Runtime Boundaries

Repository specs are development records. Runtime workload state remains profile-
scoped session JSON, structured plans and durable daemon task records. This
migration introduces no Projects/Tasks/Epics database or workload-state migration.

Retain the single Rust binary/library, local Git worktrees, explicit tool policy,
audit and reconciliation, and shared plain/TUI reducer. Use ordinary task branches
under feature/ or fix/ as appropriate. A task request grants safe workflow authority;
prospective approval still governs instruction changes and destructive production
operations. Repository-host and environment protections remain authoritative.

## Review, Verification and Acceptance

Every change receives self-review. Governed instructions, security, data integrity,
public interfaces and release controls always need independent exact-candidate
review; other non-trivial changes need it except documented small low-risk changes
with focused tests. Resolve every finding by implementation or reviewer-agreed
rejection. Renew affected review after relevant edits.

Use the [gate map](gates.md) during coherent changes and review fixes. Unknown
scope or freshness uses aggregate fallback. Once records, generated artifacts,
documentation, review and fixes stabilize, freeze the candidate and run
`task verify` (task check and task test exactly once), plus applicable gates.
Verify actual combined integration/main revisions; do not transfer evidence to
changed revisions without native freshness proof. Each acceptance criterion has
supporting objective evidence; subjective product decisions go to the user.

## Version and Memory Completion

Follow [spec versioning](../../specs/versioning.md) and workspace/releases.json.
Reserve before implementation; apply once at development-start or merge before
integration artifacts. At 0.y.z nib uses minor for incompatible changes and additions,
patch for compatible fixes; major advances to a stable 1.0.0 contract. Historical
specs are explicitly listed and do not receive retroactive bumps or invented tags.
The current next release is reserved at merge; local development keeps 0.1.0.

Each completed task classifies Memory Impact as updated or none with rationale.
Updated references a category file and the changelog; pending is permitted only
for genuinely unresolved active work. Version and memory gates run through Task.
