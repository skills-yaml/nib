# Development Specifications

Group every non-legacy specification by lifecycle state and one lowercase
hyphen-case primary feature:

```text
workspace/specs/<state>/<primary-feature>/<spec>.md
```

## Lifecycle

```text
backlog -> development -> test -> done
```

Development means implementation is active. Test requires confirmed shared
test integration. Done requires confirmed production release and resolved
memory impact. Branch names and coordination WIP statuses are not lifecycle
evidence.

## Status Catalog

Maintain one root catalog row per current spec with its link, primary feature,
state, and current-state rationale. Update the spec and catalog together for
every lifecycle or feature change.

## Version Impact

Every non-legacy spec declares a Version Impact table with Component, Impact,
Release, and Rationale columns. Follow the pinned standard's `versioning.md`.
Use major, minor, patch, or justified none. Register release IDs and member paths
in `workspace/releases.json`; apply the shared bump once at development start
or merge. Keep member paths synchronized with this catalog during state moves.
