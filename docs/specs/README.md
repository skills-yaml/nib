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

Foundational specs (FT-001, FT-002, product.md, T001) were updated in place during the workspace-docs adoption + FT-001 implementation.

FT-003 and FT-004 were moved to `done/` upon merge of the implementing branch (PR #1).

Future work should use the canonical `backlog/`, `development/`, `done/` directories.

See `docs/projects/nib/inventory.md` for adoption details.
