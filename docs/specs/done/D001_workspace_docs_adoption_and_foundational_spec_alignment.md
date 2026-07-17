# D001: Workspace Docs Adoption + Foundational Spec Alignment

**Status:** Done

**Date:** 2026-06-19  
**Related:** AGENTS.md, docs/projects/nib/inventory.md, docs/specs/README.md, FT-001, FT-002, T001

## Summary

The original workspace-docs adoption aligned foundational specs with project technical
documentation. The 2026-07-15 audit upgrades that adoption to the repository-pinned
`workspace-docs@1.2.0` standard and reconciles Rust-era paths and canonical spec state.

## Original 2026-06-19 Actions

This section records the first adoption pass. The later 2026-07-15 reconciliation
below migrated lifecycle-managed specs to the canonical state directories and is the
authoritative current state.

- Updated statuses and cross-references in:
  - `docs/specs/feature/ft_001_basic_agent_tools.md`
  - `docs/specs/feature/ft_002_base_architecture.md`
  - `docs/specs/foundation/product.md`
  - `docs/specs/task/T001_implement_core_agent_tools.md`
- Added "Implementation Status" and "Post-execution notes" sections reflecting as-built reality (see architecture.md and current source in tools/, core/, context/).
- Removed outdated "(to be created)" references now that `docs/tech/permissions.md`, `architecture.md`, etc. exist and are complete.
- Aligned tool spec descriptions (list_directory, grep) with actual (basic) implementations while keeping target interfaces.
- Updated meta docs:
  - `docs/specs/README.md` (documented the alignment, clarified legacy path handling)
  - `docs/projects/nib/inventory.md` (marked legacy paths as aligned, added run notes, updated review date)
- Legacy files were left in place during that original pass. The 2026-07-15 audit
  subsequently migrated them into `backlog/`, `development/`, or `done/`.
- Canonical directories (`backlog/`, `development/`, `done/`) became the required
  lifecycle representation; this file later moved through `development/` during the
  implementation audit before returning to `done/`.

## Validation

- Changes are documentation-only (no code behavior impact).
- All links in updated specs point to existing files under `docs/tech/`.
- Future specs and tasks should follow the three-state model and reference the tech references + AGENTS.md.

This completes the alignment of early specs to the expanded documentation set.

## Reopened Audit (2026-07-15)

Scope: reconcile the workspace-docs version, inventory, Rust-era paths, canonical spec
states, and broken links that have drifted since the original adoption.

Affected areas: `docs/standards/workspace-docs/`, `docs/projects/nib/`,
`docs/specs/`, `docs/tech/`, and the documentation validation tasks.

Acceptance criteria:
- All current project guidance names `workspace-docs@1.2.0`; historical memory entries
  may retain the version in effect when they were recorded.
- Inventory and architecture references describe the current Rust-only repository.
- Internal Markdown links resolve to existing files and current spec locations.
- A deterministic documentation validation gate prevents recurrence.

Validation gates: `task docs:check`, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Keep the workspace-docs lifecycle, project inventory, links, and spec-state claims
consistent with the Rust repository without rewriting historical decisions as current truth.

### Acceptance Criteria

- [x] Canonical specs use only `backlog/`, `development/`, or `done/` state directories.
- [x] Documentation validation detects broken local links, duplicate IDs, missing
  development fields, unchecked done criteria, and state/status contradictions.
- [x] The inventory identifies reopened specs as development work.
- [x] Repository-wide final checks have been run after this reconciliation.

### Affected Areas

`AGENTS.md`, `docs/specs/`, `docs/projects/nib/inventory.md`, `docs/tech/`,
`agents/memory/`, `Taskfile.yml`, and `tests/docs_integrity.rs`.

### Implementation Evidence

- `Taskfile.yml` defines `docs:check` as the deterministic documentation gate.
- `tests/docs_integrity.rs` implements the five documented lifecycle/link invariants.

### Validation Evidence

The named tests are `internal_markdown_links_resolve`, `spec_ids_are_unique_across_states`,
`done_specs_do_not_claim_open_acceptance_items`,
`development_specs_have_required_execution_fields`, and
`explicit_spec_status_matches_state_directory` in `tests/docs_integrity.rs`.
All five passed through `task docs:check` on 2026-07-15 after reconciliation.

### Validation Gates

- [x] `task docs:check` after all reconciliation edits (5 passed on 2026-07-15).
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

None within the adopted documentation lifecycle. Empty legacy `feature/` and `task/`
directories remain only as compatibility landmarks.
