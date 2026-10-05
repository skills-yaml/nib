# Workspace Process & Governance (`workspace-docs@4.0.0`)

This document defines the process rules and operational boundaries for projects
adopting `workspace-docs@4.0.0`.

## 1. Operational Areas

- **Root Policy & Tokens**: `AGENTS.md` and `DESIGN.md` reside at the repository root.
- **Root Container Directory (`workspace/`)**: All domain directories (`agents/`, `instructions/`, `specs/`, `docs/`, `company/`) reside under `./workspace/`.
- **System Instructions (`workspace/instructions/`)**: Read-only policies and architecture rules during feature development.
- **Development Specs (`workspace/specs/`)**: Spec-driven lifecycle tracking (`backlog` $\rightarrow$ `development` $\rightarrow$ `done`) grouped by primary feature and indexed in the root specs README.
- **Human Docs (`workspace/docs/`)**: Current-project reference documentation, architecture notes, and session work logs.
- **Company Context (`workspace/company/`)**: Business reference materials, brand guidelines, design assets, domain glossaries, and strategy briefs.
- **Durable Memory (`workspace/agents/memory/`)**: Stable facts, decisions, preferences, open questions, and changelog.

## 2. Task Lifecycle

For every user-directed task or bounded work item:

1. Read current policy, the root specs README, relevant specs, and durable
   memory before acting.
2. Determine whether the work is non-trivial. If it is, create or move a spec
   to `workspace/specs/development/<primary-feature>/` and set its memory impact
   to `pending`.
3. Add or update the spec's root catalog row with the same primary feature,
   current state, and a concise rationale for that state.
4. Plan affected areas, validation, documentation, and potential durable memory
   impact.
5. Implement within the authorized scope and preserve unrelated changes.
6. Run the repository's required Taskfile gates, including spec-catalog and
   memory-impact validation.
7. Classify the completed task's memory impact as `updated` or `none`.
8. If `updated`, append the durable entry and its changelog record. If `none`,
   record a concise rationale without creating a placeholder memory entry.
9. Resolve the spec's memory-impact section and move it to
   `done/<primary-feature>/` only when all completion gates pass.
10. Update the catalog state and status rationale in the same change as the
    move.
11. State the memory-impact classification and rationale in the final handoff.

Internal commands and tool calls are implementation steps within a task, not
independent memory actions.

## 3. Spec Catalog Boundary

Each non-legacy spec has one primary feature and exactly one catalog row. The
catalog must be updated when a spec is created, moves state, changes feature,
is reopened, is superseded, or materially changes why it is in its state.

## 4. Completion Boundary

Work is not complete until implementation, validation, documentation, spec
path, root catalog state and rationale, and memory impact agree. A stale catalog
or a final handoff with missing or `pending` memory impact is incomplete.
