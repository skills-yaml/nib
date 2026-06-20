# D001: Workspace Docs Adoption + Foundational Spec Alignment

**Date:** 2026-06-19  
**Related:** AGENTS.md, docs/projects/nib/inventory.md, docs/specs/README.md, FT-001, FT-002, T001

## Summary

As part of adopting `workspace-docs@1.0.0` (see agents/memory/decisions.md) and during implementation of basic agent tools (FT-001 on `feat/implement-basic-agent-tools`), the foundational specs were aligned to the new/completed technical documentation.

## Actions Taken

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
- Legacy files left in place (no moves) per original adoption guidance. Explicit user request served as the "migration request".
- Canonical directories (`backlog/`, `development/`, `done/`) now preferred for new specs. This file is the first entry in `done/`.

## Validation

- Changes are documentation-only (no code behavior impact).
- All links in updated specs point to existing files under `docs/tech/`.
- Future specs and tasks should follow the three-state model and reference the tech references + AGENTS.md.

This completes the alignment of early specs to the expanded documentation set.