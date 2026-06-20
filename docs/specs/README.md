# Specs

nib follows `workspace-docs@1.0.0` for new spec state management.

Canonical state directories:

- `backlog/`: accepted ideas that are not actively being implemented.
- `development/`: active work with scope, acceptance criteria, affected areas, implementation plan, validation gates, and risks.
- `done/`: completed work with final behavior and validation recorded.

Allowed transitions:

- `backlog -> development`
- `development -> done`

Legacy or reference spec paths preserved during adoption (and subsequent alignment):

- `docs/specs/feature/`
- `docs/specs/foundation/`
- `docs/specs/task/`

Foundational specs (FT-001, FT-002, product.md, T001) were updated in place during the workspace-docs adoption + FT-001 implementation to align with finalized tech documentation (`architecture.md`, `permissions.md`, `ecosystem_integration.md`, etc.), current implementation reality, and the canonical spec states.

Per explicit request, content, statuses, cross-references, and "Implementation Status" notes were refreshed (no file moves to preserve history). Future work should use `backlog/`, `development/`, `done/` for new specs.

See `docs/projects/nib/inventory.md` for adoption details.
