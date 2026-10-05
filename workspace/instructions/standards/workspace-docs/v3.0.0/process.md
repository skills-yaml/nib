# Workspace Process & Governance (`workspace-docs@3.0.0`)

This document defines the process rules and operational boundaries for projects
adopting `workspace-docs@3.0.0`.

## 1. Operational Areas

- **Root Policy & Tokens**: `AGENTS.md` and `DESIGN.md` reside at the repository root.
- **Root Container Directory (`workspace/`)**: All domain directories (`agents/`, `instructions/`, `specs/`, `docs/`, `company/`) reside under `./workspace/`.
- **System Instructions (`workspace/instructions/`)**: Read-only policies and architecture rules during feature development.
- **Development Specs (`workspace/specs/`)**: Spec-driven lifecycle tracking (`backlog` $\rightarrow$ `development` $\rightarrow$ `done`).
- **Human Docs (`workspace/docs/`)**: Current-project reference documentation,
  architecture notes, and session work logs.
- **Company Context (`workspace/company/`)**: Business reference materials, brand guidelines, design assets, domain glossaries, and strategy briefs.
- **Durable Memory (`workspace/agents/memory/`)**: Stable facts, decisions, preferences, open questions, and changelog.

## 2. Task Lifecycle

For every user-directed task or bounded work item:

1. Read current policy, relevant specs, and durable memory before acting.
2. Determine whether the work is non-trivial. If it is, create or move a spec to
   `workspace/specs/development/` and set its memory impact to `pending`.
3. Plan affected areas, validation, documentation, and potential durable memory
   impact.
4. Implement within the authorized scope and preserve unrelated changes.
5. Run the repository's required Taskfile gates.
6. Classify the completed task's memory impact as `updated` or `none`.
7. If `updated`, append the durable entry and its changelog record. If `none`,
   record a concise rationale without creating a placeholder memory entry.
8. Resolve the spec's memory-impact section and move it to `done` only when all
   completion gates pass.
9. State the memory-impact classification and rationale in the final handoff.

Internal commands and tool calls are implementation steps within a task, not
independent memory actions.

## 3. Completion Boundary

Work is not complete until implementation, validation, documentation, spec
state, and memory impact agree. A final handoff or done spec with missing or
`pending` memory impact is incomplete.
