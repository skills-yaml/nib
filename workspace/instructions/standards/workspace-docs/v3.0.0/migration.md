# Migration Notes: workspace-docs@3.0.0

These notes define the breaking completion-gate change for repositories moving
from `workspace-docs@2.1.0` to `workspace-docs@3.0.0`.

Agents must first follow the canonical
[Agent Migration Guide](../AGENT_MIGRATION.md). That guide defines discovery,
project-local safety, collision handling, validation, and handoff. This document
defines the 3.0 memory-impact contract.

## Breaking Change

Version 3.0 requires agents to classify the durable-memory impact of every
completed user-directed task or bounded work item. The classification is a
completion gate, not an optional documentation suggestion.

Use one resolved status:

- `updated`: the task created or changed durable context and the appropriate
  memory category file plus `changelog.md` were updated.
- `none`: the task created no new durable context and the handoff or spec states
  why.

`pending` is allowed only while a non-trivial spec is in `development`.
Internal tool calls and commands are part of the containing task; they do not
each require an independent classification.

## Updating from 2.1

1. Create a migration spec in `workspace/specs/development/` with
   `Memory Impact` set to `pending`.
2. Install the complete 3.0.0 standard package from a trusted source.
3. Replace only the generated `AGENT-CONTEXT` block in `AGENTS.md` with the
   3.0.0 template. Preserve manual policy outside the markers.
4. Confirm all canonical memory files from `manifest.yaml` exist.
5. Update the repository's completion workflow so each final handoff states the
   resolved memory-impact status and rationale.
6. Add a deterministic Taskfile gate that rejects done specs with missing,
   `pending`, or malformed memory-impact sections.
7. Require `updated` done specs to identify one category memory file and
   `workspace/agents/memory/changelog.md`.
8. Classify existing current done specs from their recorded outcomes. Reference
   existing memory entries where durable context was already captured; do not
   invent a new durable entry solely to retrofit the classification.
9. Resolve the migration spec's memory impact and move it to `done` only after
   all required gates pass.

## Fresh Adoption

Fresh adoptions use the same target structure as 2.1, plus the required memory
files and memory-impact completion gate declared in the 3.0.0 manifest.

Initialize memory category files without inventing project facts. The adoption
itself normally produces a durable decision: record the adopted version in
`decisions.md` and add the corresponding `changelog.md` entry.

## Validation Checklist

1. Compare the repository with [`manifest.yaml`](./manifest.yaml) and
   [`audit-checklist.md`](./audit-checklist.md).
2. Confirm `AGENTS.md` contains one balanced generated block pinned to 3.0.0.
3. Confirm every canonical memory file exists and contains no secrets or
   transient scratch notes.
4. Confirm new development specs initialize memory impact as `pending`.
5. Confirm work cannot move to `done` until memory impact is `updated` or `none`
   with a rationale.
6. Confirm `updated` specs identify the category file and `changelog.md`.
7. Run `task check`, `task test`, and all project-specific gates.
8. Review the diff for accidental product, security, infrastructure, CI/CD,
   secret, or unrelated-file changes.

## Rollback

If the new completion gate cannot be integrated safely, keep the repository
pinned to its previous concrete version and document the blocker. Restore only
the files changed by the attempted migration using the repository's normal
version-control workflow. Never discard unrelated user changes.
