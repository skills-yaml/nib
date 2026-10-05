# Agent Migration Guide

This guide is the canonical playbook for agents that need to adopt or update
the workspace documentation structure in a repository. It covers process and
safety; use the target version's migration notes for version-specific path
changes.

The current recommended target is `workspace-docs@5.0.0`:

- [Version-specific migration notes](./v5.0.0/migration.md)
- [Required structure manifest](./v5.0.0/manifest.yaml)
- [Generated `AGENTS.md` context template](./v5.0.0/agents-template.md)
- [Specs README template](./v5.0.0/specs-readme-template.md)
- [Adoption audit checklist](./v5.0.0/audit-checklist.md)

## Choose the Migration Mode

Classify the current repository before changing files.

| Current state | Mode | Required action |
| --- | --- | --- |
| No workspace documentation structure | Fresh adoption | Add root governance, the `workspace/` container, and project-local documentation. |
| A `workspace/` container with an older version marker | Version update | Read every migration note between the adopted and target versions, then apply only the required deltas. |
| Root-level `agents/`, `instructions/`, `specs/`, or `docs/` directories | 2.0 update | Move the relevant content into the `workspace/` container using an explicit file map. |
| Older `docs/tech/`, `docs/standards/`, `docs/specs/`, or `agents/memory/` paths | 1.x update | Reclassify and move current-project content through every required intermediate layout to 5.0. |
| A 2.1 structure without the memory-impact gate | Version update | Preserve the container layout, then apply the 3.0, 4.0, and 5.0 contracts in order. |
| A 3.0 structure without feature-grouped specs and a root status catalog | Version update | Apply the 4.0 categorization contract, then add the 5.0 test state. |
| A 4.0 structure without a test state | Version update | Add the test directory and separate test integration from production release. |
| A 5.0 structure with missing or inconsistent files | Repair | Compare against the 5.0 manifest and repair only the documented gaps. |

If the state is mixed, treat it as an update. Do not scaffold a second copy of
content that already exists.

## Non-Negotiable Safety Rules

Agents must follow these rules throughout the migration:

- Work only inside the current repository unless the user explicitly expands
  the scope.
- Do not inspect, inventory, copy, or publish information about sibling
  projects or the host machine.
- Use repository-relative paths in documentation, specs, memory, commits, pull
  requests, issues, and handoff notes.
- Preserve user changes and unrelated work already present in the worktree.
- Preserve manual rules outside `AGENT-CONTEXT` markers in `AGENTS.md`.
- Never copy secrets, credentials, environment values, generated caches, or
  machine-specific state into the workspace.
- Do not run broad or destructive moves. Resolve every source and destination
  first, and move one explicit area at a time.
- Do not suppress migration failures. A missing source, occupied destination,
  or content collision must be inspected and handled deliberately.
- Do not retain central cross-project inventories from older layouts. Keep only
  documentation that belongs to the current project.

## Phase 1: Discover the Current State

1. Read the current repository's `AGENTS.md`, `DESIGN.md`, `README.md`, task
   guidance, SDLC guidance, root specs README, relevant specs, and durable
   memory.
2. Inspect the current worktree and branch. Separate pre-existing user changes
   from migration changes before editing.
3. Search the current repository for `workspace-docs@` declarations and legacy
   relative paths. Do not search outside the repository.
4. Compare existing files and directories with the target
   [`manifest.yaml`](./v5.0.0/manifest.yaml).
5. Identify which current-project documents are authoritative, duplicated,
   obsolete, or unclassified.
6. Confirm the target version. Resolve `default` or `latest` for discovery, but
   pin the concrete version in generated context and migration records.
7. Create or move a migration spec into
   `workspace/specs/development/<primary-feature>/` before a non-trivial
   migration. Register it in `workspace/specs/README.md` with its current state
   and rationale. The spec must include scope, acceptance criteria, affected
   areas, validation gates, rollback risks, and a `Memory Impact` section with
   `Status: pending`.

## Phase 2: Build an Explicit Migration Map

Record every intended source, destination, merge decision, and validation in
the development spec. Use this 5.0 mapping as the baseline:

| Existing purpose or legacy location | 5.0 destination |
| --- | --- |
| Root execution policy | `AGENTS.md` |
| Root design tokens and UI policy | `DESIGN.md` |
| Repository overview | `README.md` |
| Durable memory | `workspace/agents/memory/` |
| Reusable technical guidance | `workspace/instructions/tech/` |
| Versioned workspace standards | `workspace/instructions/standards/` |
| Agent prompts and role instructions | `workspace/instructions/agents/` |
| Skill policy and guidance | `workspace/instructions/skills/` |
| Active and completed specs | `workspace/specs/<state>/<primary-feature>/`, indexed with state and rationale in `workspace/specs/README.md` |
| Historical current-project specs | `workspace/specs/legacy/` |
| Architecture reference | `workspace/docs/architecture/` |
| Work and research notes | `workspace/docs/work/` |
| Business, brand, domain, and strategy context | `workspace/company/` |

Classify files by purpose, not only by their old directory name. If a source and
destination both contain content, plan a merge; never overwrite one with the
other.

## Phase 3: Apply the Migration

### Fresh Adoption

1. Keep or create the required root files from the target manifest.
2. Create the required `workspace/` directories. A reserved directory may use a
   short `README.md` explaining its purpose until project-specific content
   exists.
3. Create
   `workspace/specs/README.md` from `specs-readme-template.md`, define the
   migration spec's primary feature, and register its state and rationale.
4. Add the selected version package and applicable technical instructions from
   an explicit, trusted source supplied for the migration. Do not discover a
   copy from another local project.
5. Add the generated context block from the target `agents-template.md` to
   `AGENTS.md`, while keeping all project-specific manual rules outside it.
6. Initialize durable memory with current-project facts only.
7. Add Taskfile entrypoints that call the repository's real validation and test
   commands as required by `workspace/instructions/tech/task.md`.
8. Add the memory-impact gate and record the adoption decision in the
   appropriate memory category plus `changelog.md`.
9. Add the feature-grouped spec layout, including `test`, the root catalog, and
   deterministic catalog validation.
10. Document the project's test and production branches or environments; use
    `develop` and `main` only when they are the actual conventions.

### Updating an Existing Adoption

1. Read the migration notes for each version boundary between the pinned and
   target versions. Do not skip an intermediate breaking change.
2. Create required destinations before moving content.
3. Move files according to the explicit migration map and verify each move
   before continuing.
4. Merge duplicate documents by preserving current authoritative content and
   removing stale copies only after references are updated.
5. Replace only the generated `AGENT-CONTEXT` block with the target template.
   Preserve manual rules byte-for-byte unless the user requested changes.
6. Update repository-relative links, Taskfile paths, CI references, and tooling
   references affected by the move.
7. Reconcile memory: preserve stable current-project facts and decisions,
   supersede obsolete entries, and remove secrets or unrelated local context.
8. Add or update the deterministic memory-impact gate required by the target
   version.
9. Categorize current specs, update the root catalog, and add or update the
   deterministic spec-catalog gate required by the target version.
10. Preserve production-released specs in done. Keep active implementation in
    development and move integrated but unreleased work to test with evidence.
11. Remove empty legacy directories only after confirming no current-project
   material was lost.

For a 1.x or 2.0 migration, apply the structural mapping in
[`v2.1.0/migration.md`](./v2.1.0/migration.md), the 3.0 completion contract in
[`v3.0.0/migration.md`](./v3.0.0/migration.md), the 4.0 catalog contract in
[`v4.0.0/migration.md`](./v4.0.0/migration.md), and the 5.0 test-stage contract
in [`v5.0.0/migration.md`](./v5.0.0/migration.md). Start at the first version
newer than the repository's current pin and do not skip intermediate notes.

## Phase 4: Validate the Result

Validation must be proportional to the adopting repository, but it must include
all of the following:

1. Compare the final tree with the target manifest and audit checklist.
2. Confirm there is exactly one active location for instructions, specs, docs,
   and durable memory.
3. Confirm generated context markers are balanced and identify the pinned
   version.
4. Check all current-document relative links.
5. Search the current repository for obsolete version markers and legacy path
   references. Keep historical references only when they are clearly labeled
   and still safe.
6. Run the repository's Taskfile gates, normally `task check` and `task test`,
   plus stack-specific gates required by the affected project.
7. Run the local-information/privacy gate when the repository provides one.
8. Confirm every development, test, and done spec required by the target
   contract has a valid memory-impact classification, and confirm the Taskfile
   gate rejects missing or unresolved classifications.
9. Confirm every non-legacy spec has one valid primary-feature path and one
   matching root catalog row with its current state and rationale.
10. Confirm test rationales identify shared integration and done rationales
    identify production release using the configured project topology.
11. Review the full diff for unintended product, authentication, data-flow,
   infrastructure, CI/CD, or secret changes.
12. Record exact validation results and any justified deviations in the
   migration spec.

Do not mark the migration complete while a required gate is failing or a
content collision remains unresolved.

## Phase 5: Complete and Hand Off

1. Classify the migration's memory impact as `updated` or `none`; adoption and
   version changes normally require a durable decision entry and changelog
   record.
2. Keep the migration spec in development until its implementation is merged
   or deployed to the configured test target. Then move it to
   `workspace/specs/test/<primary-feature>/` with integration evidence.
3. Move the test spec to `workspace/specs/done/<primary-feature>/` only after
   production release, all acceptance criteria and validation gates pass,
   memory impact is resolved, and the root catalog has the release rationale.
4. Record the adopted version and any durable project-specific decision in
   `workspace/agents/memory/`.
5. Confirm the worktree contains only intended changes.
6. When authorized, commit and publish changes through the repository's
   documented SDLC.
7. Report the migration mode, previous and target versions, significant moves,
   preserved manual policy, validation results, and remaining deviations.

Use this compact handoff shape:

```text
Migration mode: fresh adoption | version update | repair
Previous version: none | workspace-docs@X.Y.Z
Target version: workspace-docs@X.Y.Z
Content moved or merged: <repository-relative paths>
Manual AGENTS.md policy preserved: yes | no, with reason
Validation: <commands and results>
Integration/release status: development | test | production
Memory impact: updated | none, with rationale
Deviations or follow-up: none | <project-local details>
```

The handoff must not contain machine-local paths, identities, sibling-project
details, credentials, or environment-specific values.

## Workspace Docs 7 Candidate

When the user explicitly targets v7, read its [SDLC](./v7.0.0/sdlc.md),
[process](./v7.0.0/process.md), [migration](./v7.0.0/migration.md), and
[audit checklist](./v7.0.0/audit-checklist.md). These override older lifecycle
and branch-authority wording in this guide for the v7 target only. Add blocked
resume metadata; define done as verified main merge with reconciled acceptance
and records; track publication separately. Run modular gates during changes
and full verification after review/fixes stabilize the candidate. Preserve
released packages, historic evidence, and manual policy outside approved scope.
The default pointer still selects v5 unless an explicit successor is requested.
