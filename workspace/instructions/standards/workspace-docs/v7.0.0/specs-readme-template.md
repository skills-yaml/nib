# Development Specifications

Group every non-legacy spec under
`workspace/specs/<state>/<primary-feature>/<spec>.md`. Define lowercase
hyphen-case primary features here and keep one matching catalog row per spec.

## Lifecycle

Use backlog, development, test, blocked, or done. The default route is
backlog -> development -> test -> done; documented direct routes permit
verified development -> done after main merge. Done requires reconciled
acceptance, verification, documentation, catalog, versions, and memory;
publication is separate. Blocked work records Previous State, Block Kind
(impediment or deferred), Block Reason, and Resume Condition. Resume at the
previous stage and renew stale evidence. Preserve done history with linked
follow-up specs. Never infer lifecycle progress from branch names alone.

## Status Catalog

Define primary feature categories and maintain one catalog row with its spec
link, primary feature, state, and status rationale. Synchronize the spec,
catalog, and release member paths on every transition.

<!-- SPEC-CATALOG:START -->
| Spec | Primary feature | State | Status rationale |
| --- | --- | --- | --- |
<!-- SPEC-CATALOG:END -->

## Version Impact

Every current spec includes Component, Impact, Release, and Rationale under
[Spec Versioning](./versioning.md). Reserve one owner/target per component
before implementation; apply once before integration artifacts. Applied
versions suffice for done; record publication separately as released.
