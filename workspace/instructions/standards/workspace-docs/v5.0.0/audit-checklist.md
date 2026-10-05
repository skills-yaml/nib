# Workspace Docs Audit Checklist

Use this checklist to compare a project against `workspace-docs@5.0.0`.

## Required Structure

- [ ] Root `AGENTS.md`, `DESIGN.md`, and `README.md` exist.
- [ ] Required task, SDLC, and project-structure instructions exist.
- [ ] Agent, skill, and versioned-standard instruction directories exist.
- [ ] `workspace/specs/README.md` exists.
- [ ] `workspace/specs/backlog/`, `development/`, `test/`, `done/`, and
  `legacy/` exist.
- [ ] Required durable-memory files exist.

## Spec Lifecycle

- [ ] Every non-legacy spec uses
  `workspace/specs/<state>/<primary-feature>/<spec>.md`.
- [ ] State is exactly `backlog`, `development`, `test`, or `done`.
- [ ] Every primary feature is lowercase hyphen-case and defined in the root
  specs README.
- [ ] Every non-legacy spec has exactly one matching catalog row.
- [ ] Every catalog row includes a non-empty evidence-based status rationale.
- [ ] Active implementation has a development spec with required scope,
  acceptance criteria, affected areas, gates, risks, and memory impact.
- [ ] Test specs have confirmed integration into the configured test branch or
  environment; the conventional branch is `develop`.
- [ ] Done specs have confirmed release through the configured production
  branch or environment; the conventional branch is `main`.
- [ ] Branch names are treated as configurable conventions rather than proof by
  themselves.
- [ ] Catalog rows change in the same change as lifecycle paths and declared
  states.

## Agent Context and Memory

- [ ] The generated context is balanced and pinned to
  `workspace-docs@5.0.0`.
- [ ] Manual project rules remain outside generated blocks.
- [ ] Development and test specs include a `Memory Impact` section.
- [ ] Done specs resolve memory impact to `updated` or `none` with a rationale.
- [ ] Updated memory impact references a category file and `changelog.md`.
- [ ] Every completed task handoff states its resolved memory impact.
- [ ] Memory contains no secrets or transient scratch notes.

## Validation

- [ ] Spec structure and root-catalog validation is deterministic and required.
- [ ] The test state is covered by representative success and failure tests.
- [ ] Repository quality gates run through Taskfile.
- [ ] Required project-specific validation has passed.
- [ ] Repository content contains no machine-local or sibling-project details.
