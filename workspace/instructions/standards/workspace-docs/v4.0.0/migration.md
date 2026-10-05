# Migration Notes: workspace-docs@4.0.0

These notes define the breaking spec-organization and catalog change for
repositories moving from `workspace-docs@3.0.0` to `workspace-docs@4.0.0`.

Agents must first follow the canonical
[Agent Migration Guide](../AGENT_MIGRATION.md). That guide defines discovery,
project-local safety, collision handling, validation, and handoff.

## Breaking Change

Version 4.0 requires every non-legacy spec to live at
`workspace/specs/<state>/<primary-feature>/<spec>.md`. It also requires
`workspace/specs/README.md` to define categories and maintain exactly one row
per current spec with its link, primary feature, state, and status rationale.

The catalog must change with spec creation, lifecycle transitions, primary
feature changes, reopening, supersession, or material changes to the reason a
spec is in its state.

## Updating from 3.0

1. Create or move the migration spec to
   `workspace/specs/development/workspace-governance/` and initialize memory
   impact to `pending`.
2. Install the complete 4.0.0 package from a trusted source.
3. Replace only the generated `AGENT-CONTEXT` block with the 4.0.0 template;
   preserve manual policy outside the markers.
4. Inventory current non-legacy specs, assign each one primary feature, and
   define those features in `workspace/specs/README.md`.
5. Move each spec under the matching state and feature without changing its
   lifecycle state. Update relative links affected by the extra path level.
6. Build the canonical status catalog with exactly one row per current spec and
   an evidence-based reason for its present state.
7. Update task, SDLC, project-structure, migration-skill, and repository
   guidance to require synchronized catalog changes.
8. Add a deterministic Taskfile gate for category paths, declared state,
   category definitions, and catalog coverage and metadata.
9. Preserve the 3.0 memory-impact gate, run all required checks, resolve the
   migration spec's memory impact, move it to the same feature under `done`,
   and update its catalog row in the same change.

## Fresh Adoption

Create `workspace/specs/README.md` from `specs-readme-template.md`. Define the
first primary feature before creating the first spec. Register the initial
migration spec in the catalog when creating it, then update the row whenever
its state or status rationale changes.

## Validation Checklist

1. Compare the repository with `manifest.yaml` and `audit-checklist.md`.
2. Confirm every non-legacy spec has exactly one state, primary feature, and
   catalog row.
3. Confirm every catalog state and feature matches the spec path and declared
   `State` field.
4. Confirm every catalog rationale is non-empty and explains the current state.
5. Confirm `AGENTS.md` contains one generated block pinned to 4.0.0.
6. Confirm memory-impact requirements from 3.0 remain enforced.
7. Run `task check`, `task test`, and all project-specific gates.
8. Review the diff for unintended application, security, infrastructure,
   CI/CD, secret, or unrelated-file changes.

## Rollback

If feature categorization or catalog validation cannot be integrated safely,
keep the repository pinned to 3.0.0 and document the blocker. Restore only the
files changed by the attempted migration using normal version control and never
discard unrelated user changes.
