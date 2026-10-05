# Specifications README Template

Use this template for `workspace/specs/README.md`.

```md
# Development Specifications

Every non-legacy spec uses
`workspace/specs/<state>/<primary-feature>/<spec>.md`, where state is `backlog`,
`development`, `test`, or `done` and primary feature is lowercase hyphen-case.

The normal flow is `backlog -> development -> test -> done`. Test means
confirmed integration into the configured test branch or environment,
conventionally `develop`. Done means confirmed production release,
conventionally through `main`. Repository-local equivalents are allowed.

## Feature Categories

| Primary feature | Purpose |
| --- | --- |
| `workspace-governance` | Workspace adoption, documentation policy, lifecycle governance, and validation. |

## Status Catalog

Update the catalog whenever a spec is created, moves state, changes feature, is
reopened, is superseded, or materially changes why it is in its state.

<!-- SPEC-CATALOG:START -->
| Spec | Primary feature | State | Status rationale |
| --- | --- | --- | --- |
<!-- SPEC-CATALOG:END -->
```
