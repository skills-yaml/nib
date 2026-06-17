# Specs

nib follows `workspace-docs@1.0.0` for new spec state management.

Canonical state directories:

- `backlog/`: accepted ideas that are not actively being implemented.
- `development/`: active work with scope, acceptance criteria, affected areas, implementation plan, validation gates, and risks.
- `done/`: completed work with final behavior and validation recorded.

Allowed transitions:

- `backlog -> development`
- `development -> done`

Legacy or reference spec paths preserved during adoption:

- `docs/specs/feature/`
- `docs/specs/foundation/`
- `docs/specs/task/`

Do not move or rewrite legacy specs unless a separate migration explicitly requests it.
