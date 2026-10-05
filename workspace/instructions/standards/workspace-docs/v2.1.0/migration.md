# Migration Notes: workspace-docs@2.1.0

These notes define the version-specific changes for repositories moving from
`workspace-docs@1.x` or `workspace-docs@2.0.0` to
`workspace-docs@2.1.0`.

Agents must first follow the canonical
[Agent Migration Guide](../AGENT_MIGRATION.md). That guide defines discovery,
project-local safety, collision handling, validation, and handoff. This document
defines only the 2.1 target and path changes.

## 2.1 Architecture Change

Version 2.1 keeps `AGENTS.md`, `DESIGN.md`, and `README.md` at the repository
root and consolidates the other workspace domains under one `workspace/`
container:

```text
repository-root/
├── AGENTS.md
├── DESIGN.md
├── README.md
└── workspace/
    ├── agents/
    │   └── memory/
    ├── instructions/
    │   ├── tech/
    │   ├── standards/
    │   ├── agents/
    │   └── skills/
    ├── specs/
    │   ├── backlog/
    │   ├── development/
    │   ├── done/
    │   └── legacy/
    ├── docs/
    │   ├── architecture/
    │   └── work/
    └── company/
        ├── documents/
        ├── design/
        ├── domain/
        └── strategy/
```

The exact required files and directories are authoritative in
[`manifest.yaml`](./manifest.yaml).

## Updating from 2.0

Version 2.0 uses the same domains at the repository root. Move each domain to
its explicit 2.1 destination only after checking for a collision:

| 2.0 source | 2.1 destination |
| --- | --- |
| `agents/` | `workspace/agents/` |
| `instructions/` | `workspace/instructions/` |
| `specs/` | `workspace/specs/` |
| `docs/` | `workspace/docs/` |

Create the required `workspace/company/` subdirectories. Populate them only
with current-project content; otherwise use their reserved-directory README
files.

Do not move all root directories with one command. Resolve and validate each
source and destination independently so an occupied target cannot be silently
overwritten.

## Updating from 1.x

Version 1.x separates technical instructions, standards, specs, memory, and
human documentation across older root paths. Use this mapping:

| 1.x source or purpose | 2.1 destination |
| --- | --- |
| `agents/memory/` | `workspace/agents/memory/` |
| `docs/tech/` | `workspace/instructions/tech/` |
| `docs/standards/` | `workspace/instructions/standards/` |
| `docs/specs/backlog/` | `workspace/specs/backlog/` |
| `docs/specs/development/` | `workspace/specs/development/` |
| `docs/specs/done/` | `workspace/specs/done/` |
| Historical current-project feature, foundation, or task specs | `workspace/specs/legacy/` |
| `docs/architecture/` | `workspace/docs/architecture/` |
| `docs/work/` | `workspace/docs/work/` |

Classify ambiguous files by purpose. Merge duplicate content deliberately, and
do not retain central inventories or information about unrelated local
projects.

## Required Policy Updates

1. Replace only the generated block between the `AGENT-CONTEXT` markers in
   `AGENTS.md` with [`agents-template.md`](./agents-template.md).
2. Preserve every manual project rule outside the generated block unless the
   user explicitly requested a policy change.
3. Pin the generated block to `workspace-docs@2.1.0`.
4. Update repository-relative links and Taskfile paths affected by the move.
5. Keep static instructions under `workspace/instructions/` read-only during
   ordinary feature work.
6. Keep specs in exactly one lifecycle state and preserve historical
   current-project specs under `workspace/specs/legacy/`.
7. Store only stable current-project facts, decisions, preferences, and open
   questions in `workspace/agents/memory/`.

## Content That Must Not Be Migrated

- Secrets, tokens, credentials, or environment values.
- Generated caches, build artifacts, dependency directories, or temporary
  scratch files.
- Machine-local paths, usernames, hostnames, or environment state.
- Sibling-project documentation or central cross-project inventories.
- Stale duplicate files whose authoritative replacement has been verified.

## 2.1 Validation Checklist

After applying the mapping:

1. Compare the repository with [`manifest.yaml`](./manifest.yaml).
2. Complete [`audit-checklist.md`](./audit-checklist.md).
3. Confirm `AGENTS.md` contains one balanced generated block pinned to 2.1.0.
4. Confirm old and new lifecycle directories do not coexist.
5. Validate current-document relative links.
6. Run `task check`, `task test`, and all project-specific gates required by the
   affected repository.
7. Review the full diff for accidental product, security, infrastructure,
   CI/CD, secret, or unrelated-file changes.
8. Record results and any justified deviations in the migration spec.

The migration is complete only when the development spec's acceptance criteria
and required validation gates pass.

## Rollback

Before moving content, record the current branch and worktree state. If a
collision or validation failure cannot be resolved safely, stop the migration
and restore only the explicitly moved paths using the repository's normal
version-control workflow. Never discard unrelated user changes.
