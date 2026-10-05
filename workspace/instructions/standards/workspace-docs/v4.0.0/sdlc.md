# Spec-Driven SDLC

`workspace-docs@4.0.0` requires feature-categorized, spec-driven development for
non-trivial work.

## Canonical Layout

Each non-legacy spec must be in exactly one state and one primary feature:

```text
workspace/
  specs/
    README.md
    backlog/<primary-feature>/<spec>.md
    development/<primary-feature>/<spec>.md
    done/<primary-feature>/<spec>.md
```

The primary feature uses lowercase hyphen-case and identifies the main
user-facing capability or governance outcome. Record secondary relationships
inside the spec instead of duplicating the file.

## Root Status Catalog

`workspace/specs/README.md` defines every category in use and contains exactly
one catalog row for every non-legacy spec. Each row includes:

- a relative link to the spec;
- its primary feature;
- its current lifecycle state;
- a concise rationale explaining why it is in that state.

Update the catalog in the same change whenever a spec is created, moves state,
changes primary feature, is reopened, is superseded, or materially changes its
status rationale.

## Backlog

`backlog/<primary-feature>` contains accepted ideas that are not actively being
implemented. A backlog spec should include the problem, value, rough scope,
known constraints, and open questions.

## Development

`development/<primary-feature>` contains active work. A development spec must
include scope, acceptance criteria, affected areas, implementation plan,
validation gates, risks when relevant, and a `Memory Impact` section with
`Status: pending`.

Agents must not start non-trivial implementation until the spec and its active
catalog row exist.

## Done

`done/<primary-feature>` contains completed work. A spec may move to done only
when implementation, acceptance criteria, validation, documentation, catalog
state and rationale, and memory impact are reconciled. Memory impact must be
`updated` or `none` with a rationale; `updated` identifies the category memory
file and `changelog.md`.

## Transitions

Allowed transitions are `backlog -> development` and `development -> done`.
Exceptional transitions require a note in the spec or durable memory. Preserve
the primary feature during a transition unless the spec's main outcome changed.

## Completion Rule

Agents must not declare work complete only because files changed. Work is
complete when the spec, feature category, catalog, implementation, validation,
documentation, and memory impact agree. Every final handoff states
`Memory impact: updated` or `Memory impact: none` with a rationale.
