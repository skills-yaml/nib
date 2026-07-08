# Specs

nib follows `workspace-docs@1.2.0` for spec state management.

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

Foundational specs (FT-001, FT-002, product.md, T001) were updated in place during workspace-docs adoption + FT-001 implementation.

FT-004 was moved to `done/` upon merge of the implementing branch (PR #1). FT-003 was **reopened** to `development/` on 2026-07-02 (never implemented).

## Active development

- [FT-005: Pure Rust Core Migration](development/ft_005_pure_rust_core_migration.md) — **Rust core shipped** (Python removed); FT-003/MCP polish remaining.
- [FT-003: Hybrid Sandboxing](development/ft_003_adopt_codex_sandboxing.md) — reopened; implement in Rust (FT-005 Phase 5 / T019).

Future work should use the canonical `backlog/`, `development/`, `done/` directories.

See `docs/projects/nib/inventory.md` for adoption details.
