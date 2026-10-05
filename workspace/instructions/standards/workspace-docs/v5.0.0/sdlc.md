# Spec-Driven SDLC

`workspace-docs@5.0.0` requires feature-categorized specs and separates local
implementation, shared testing, and production release.

## Canonical Layout

```text
workspace/specs/
  README.md
  backlog/<primary-feature>/<spec>.md
  development/<primary-feature>/<spec>.md
  test/<primary-feature>/<spec>.md
  done/<primary-feature>/<spec>.md
  legacy/
```

Every non-legacy spec has one lowercase hyphen-case primary feature and exactly
one matching root catalog row with its link, feature, state, and rationale.

## States

### Backlog

Accepted work that is not actively being implemented. Record the problem,
value, rough scope, constraints, and open questions.

### Development

Active implementation. Before work begins, record scope, acceptance criteria,
affected areas, an implementation plan, validation gates, relevant risks, and
a `Memory Impact` section initialized to `pending`.

Local implementation and local gates do not advance the spec automatically.

### Test

Implemented work confirmed as merged into the configured shared test branch or
deployed to the configured test environment. The conventional branch is
`develop`. Record integration evidence in the spec and catalog rationale, then
perform shared integration, acceptance, security, and release validation.

### Done

Work confirmed as released through the configured production branch or
environment. The conventional production branch is `main`. Done also requires
reconciled acceptance criteria, validation, documentation, catalog rationale,
and memory impact resolved to `updated` or `none`.

## Branch and Environment Mapping

`develop` and `main` are defaults, not mandatory names. A repository may define
equivalent test and production branches or deployment environments in its SDLC
policy. Lifecycle state follows confirmed integration and release events, not
the checked-out branch name alone.

## Transitions

```text
backlog -> development -> test -> done
```

Exceptional reverse or skipped transitions require an explicit explanation in
the spec or durable memory. Preserve the primary-feature directory unless the
spec's main outcome changes. Every transition updates the spec path, declared
state, catalog state, and catalog rationale together.

## Completion Rule

Agents must distinguish three outcomes:

- implementation complete: remain in development until test integration;
- test complete: remain in test until production release;
- released: move to done after every completion gate and memory requirement is
  reconciled.
