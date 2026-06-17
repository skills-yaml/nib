# CI

Metadata:

- Standard: workspace-docs@1.0.0
- Status: static
- Owner: project
- Last reviewed: 2026-06-17

## Scope

This document records continuous integration expectations for `nib`.

## Source Of Truth

- `.github/workflows/` when present.
- `Taskfile.yml` or `Taskfile.yaml` when present.
- `README.md` and `AGENTS.md` for documented quality gates.

## Required Rules

- Keep CI aligned with local task entrypoints when tasks exist.
- Do not commit secrets, tokens, private keys, or environment-specific credentials.
- Document missing or not-yet-enabled CI in `docs/projects/nib/inventory.md`.

## Workflow

1. Check available workflows under `.github/workflows/`.
2. Check task targets with `task --list` or `task --list-all` when a Taskfile exists.
3. Mirror local validation commands in CI where practical.

## Validation

- `task --list` or `task --list-all` when a Taskfile exists.
- `task check` and `task test` when defined and dependencies are available.

## References

- `AGENTS.md`
- `README.md`
- `docs/projects/nib/inventory.md`
