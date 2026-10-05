# Spec Versioning and Release Reservations

Every non-legacy spec MUST declare a `Version Impact` section before work
begins, including backlog proposals. Use this table, with one row per affected
independently released component:

```markdown
## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| example-library | minor | example-library-next | Adds a compatible public capability. |
```

## Classify the Impact

Follow [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
`major` changes a public contract incompatibly, `minor` adds compatible
functionality or deprecates public behavior, and `patch` fixes compatible
behavior. For this documentation standard, required files, workflows, and
validation gates are the public contract. Use `none` only when no versioned
artifact changes (for example, a project-local work note), with a rationale
and release `none`. It is not permission to skip a bump for shipped changes.

For each independently versioned component, choose the highest impact across
all specs intentionally included in the same release: major > minor > patch.
Bump once from its latest released baseline, not once per agent or spec.
Reset minor and patch for a major bump and patch for a minor bump. At 0.y.z,
this repository uses minor for incompatible pre-1.0 changes and additions,
patch for compatible fixes; a spec explicitly requesting the stable contract
uses major to advance to 1.0.0. Document that convention in adopting projects.

A coordinated multi-package release may use one release-unit row when its
manifest pins every member version and native package gates check those pins.
This repository's `workspace-development-toolkit` row covers that coordinated
suite, including its draft skill packages; a package released separately must
have its own reservation. Grouping never permits rewriting a released member.

The reservation ledger stores normal X.Y.Z baseline and target versions with
no leading zeroes. Pre-release identifiers and build metadata belong to native
artifact tooling; an RC consumes the reservation for its final core version,
and build metadata never creates a distinct available target. Versions are
scoped to a component; identical numbers in different components do not clash.

## Record the Release

Maintain `workspace/releases.json` with schema version 1 and a `releases` list.
Each release entry has these fields:

- `id`: unique lowercase hyphen-case reservation identifier;
- `component`: independently versioned artifact identifier;
- `baseline`, `target`: released baseline and computed next normal version;
- `impact`: aggregate major, minor, or patch;
- `timing`: `development-start` (default) or `merge`;
- `status`: `planned`, `applied`, or `released`;
- `owner`: one logical agent or release-owner identifier, never a machine identity;
- `version_source`: repository-relative manifest containing a top-level
  `version:` scalar (adopters with other formats supply a native gate);
- `specs`: exact member-spec paths, updated together with lifecycle moves;
- `evidence`: why the target is shared and, once applied, the version-source
  update; once released, the immutable tag/revision or release event.

Backlog specs may name a `planned` reservation. On entering development,
`development-start` reservations MUST update the authoritative source and
its required mirrors, lockfiles, and generated pins in the same change and
become `applied`. For `merge`, reserve before implementation and apply the bump
in the integration transaction (or its gated follow-up) before building test
or release artifacts or moving the spec to test. Merging a proposal alone does
not count as integration of its implementation. Never defer past release.

An applied version is not a published release. Keep stable aliases on released
versions and lifecycle state tied to actual integration and release evidence.
At production release, reconcile the ledger to `released` with evidence before
member specs become done. Check source versions for current reservations;
old released records remain history as the component advances.

## Serialize Claims and Prevent Double Bumps

Before claiming a target, EVERY agent checks the latest shared ledger,
component manifest, relevant tags/package registry, and active coordination
records. A worktree-local copy alone cannot prove availability. If published
state cannot be checked, record the blocker; do not publish an assumed-free
version. Read-only offline development can reuse a documented draft candidate.

No coordinator agent is required. On the same machine, peers use the shared
transactional runtime's `reserve` command before choosing a target. The board
serializes component reservations; matching release IDs share and aggregate a
single bump, while conflicting release claims fail. Each peer records the
reservation ID and checked base revision in its WIP record. Version files may
be edited in different worktrees, but integration must reconcile their combined
ledger and manifests against the current reservation and pass the version gate.
The peer applying a bump is a temporary task owner, not a central coordinator.

The initial runtime supports one local Git common directory. Separate clones,
network filesystems, and cross-host synchronization are unsupported; do not
simulate shared ownership using unmerged ledger copies or copy the local board.

When a target is taken:

1. If it is the SAME explicitly shared, unreleased release, join its membership
   and reuse its bump. An `applied` status means do not bump again.
2. If new scope raises the aggregate impact, a peer transaction recomputes the target
   from the released baseline and updates every affected pin before integration.
3. Otherwise wait for the existing component release, then recompute from the
   new released baseline. This ledger permits one open release per component.
   Never steal a reservation, silently combine independent releases, or pick
   an arbitrary free patch that understates the impact.

Recheck at handoff and immediately before integration/publication. On stale
baseline, changed membership, or conflicting owner claims, stop integration,
reconcile through the shared board, and rerun gates. Retry of an already applied
reservation is idempotent. Cancellation requires owner reconciliation of all
members; never decrement a published version or reuse a published target.

## Validation and Migration

`task versions:check`, included in `task check`, checks required spec rows,
normal-version arithmetic, unique claims, one open release per component,
member consistency, version-source agreement, and timing/lifecycle evidence.
Run it after reservations, ownership changes, and integration. The offline gate
cannot establish remote occupancy or serialize distributed writers; the peer
protocol above is mandatory in addition to validation.

On adoption, annotate historical done specs without changing released history.
An explicit `historical_specs` path list in the ledger freezes that migration
boundary. Those specs still declare their original impact and rationale, with
release `historical`; they do not receive retroactive bumps or invented tags.
New specs cannot use this exception. Legacy specs remain immutable and excluded.
Backfill active specs against actual candidate manifests and record already
applied bumps instead of incrementing them again.
