# Workspace Process & Governance (`workspace-docs@5.0.0`)

This document defines the process and operational boundaries for projects
adopting `workspace-docs@5.0.0`.

## Operational Areas

- Root policy and design tokens remain in `AGENTS.md` and `DESIGN.md`.
- Static instructions live under `workspace/instructions/`.
- Lifecycle specs live under `workspace/specs/` and are grouped by primary
  feature in `backlog`, `development`, `test`, or `done`.
- Human documentation lives under `workspace/docs/`.
- Company context lives under `workspace/company/`.
- Durable memory lives under `workspace/agents/memory/`.

## Task and Release Lifecycle

1. Register accepted work in `backlog/<primary-feature>/` when it is not active.
2. Before non-trivial implementation, move or create the spec in
   `development/<primary-feature>/`, initialize memory impact to `pending`, and
   synchronize the root catalog.
3. Implement on the authorized feature or fix branch and run local gates.
4. Keep the spec in development until the implementation is confirmed merged
   into the configured test branch or deployed to the test environment.
5. Move the spec to `test/<primary-feature>/` and update its catalog rationale
   with integration evidence. The conventional test branch is `develop`.
6. Run shared integration, acceptance, security, and release validation.
7. Keep the spec in test until the change is confirmed released through the
   configured production branch or environment.
8. Resolve memory impact, move the spec to `done/<primary-feature>/`, and update
   its catalog rationale with production-release evidence. The conventional
   production branch is `main`.
9. State validation, release status, and memory impact in the handoff.

Repositories may define equivalent branch or environment names. A local branch
name alone never proves integration or release.

## Catalog Boundary

Each non-legacy spec has one primary feature and exactly one catalog row. Move
the spec, update its declared state, and update the catalog state and rationale
together.

## Completion Boundary

Implementation can be locally complete while its spec remains in development.
Test validation can pass while its spec remains in test awaiting production.
The `done` state is reserved for production-released work with reconciled
acceptance criteria, validation, documentation, catalog, and memory impact.
