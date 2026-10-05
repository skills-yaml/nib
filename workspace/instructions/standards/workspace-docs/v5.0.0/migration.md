# Migration Notes: workspace-docs@5.0.0

These notes define the breaking lifecycle change from `workspace-docs@4.0.0`
to `workspace-docs@5.0.0`. Follow the canonical
[Agent Migration Guide](../AGENT_MIGRATION.md) first.

## Breaking Change

Version 5.0 adds `test` between development and done:

```text
backlog -> development -> test -> done
```

Test indicates confirmed integration into the configured shared test branch or
environment, conventionally `develop`. Done indicates confirmed production
release through the configured production branch or environment,
conventionally `main`. Projects may document equivalent names. Branch names
alone are not lifecycle evidence.

## Updating from 4.0

1. Create or move the migration spec to
   `workspace/specs/development/workspace-governance/` and register its
   development rationale in the root catalog.
2. Install the complete 5.0.0 package from a trusted source.
3. Replace only the generated `AGENT-CONTEXT` block with the 5.0.0 template and
   preserve manual rules outside it.
4. Create `workspace/specs/test/README.md` and add the test state to structure,
   catalog, memory, and validator rules.
5. Document the repository's test and production integration points. Use
   `develop` and `main` only when they are the actual project conventions.
6. Inventory done specs. Keep already production-released work in done; do not
   move historical releases backward merely because the test state is new.
7. Keep current implementation specs in development until integration is
   confirmed. Move integrated but unreleased work to test with evidence.
8. Update skills and automation that previously moved specs directly from
   development to done.
9. Run deterministic success and failure tests for the new state, then run all
   repository Taskfile gates.
10. Resolve memory impact for the migration task, but keep its spec in
    development until test integration. Move it to test after that event and to
    done only after production release.

## Fresh Adoption

Create all four active state directories and the root catalog from
`specs-readme-template.md`. Document the repository's branch or environment
mapping before the first transition to test or done.

## Validation Checklist

1. Compare the repository with `manifest.yaml` and `audit-checklist.md`.
2. Confirm every current spec has one valid state, feature, and catalog row.
3. Confirm test and done rationales describe integration or release evidence.
4. Confirm generated context is pinned to 5.0.0 and manual policy is preserved.
5. Confirm development and test specs are subject to memory-impact validation.
6. Run `task check`, `task test`, and all project-specific gates.
7. Review the diff for unintended application, infrastructure, CI/CD, secret,
   or unrelated-file changes.

## Rollback

If the lifecycle change cannot be integrated safely, remain pinned to 4.0.0 and
document the blocker. Revert only files changed by this migration and preserve
unrelated worktree changes.
