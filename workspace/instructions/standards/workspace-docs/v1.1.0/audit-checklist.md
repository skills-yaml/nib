# Workspace Docs Audit Checklist

Use this checklist to compare a project against `workspace-docs@1.1.0`.

## Required Files

- [ ] `AGENTS.md` exists.
- [ ] `README.md` exists.
- [ ] `docs/tech/task.md` exists.
- [ ] `docs/tech/sdlc.md` exists.
- [ ] `docs/tech/project_structure.md` exists.

## Conditional Files

- [ ] Python backend projects have `docs/tech/backend_python.md`.
- [ ] CI-enabled projects have `docs/tech/ci.md`.
- [ ] Next.js projects have `docs/tech/frontend_nextjs.md`.
- [ ] Flutter projects have `docs/tech/frontend_flutter.md`.
- [ ] Infrastructure projects have `docs/tech/infrastructure.md`.

## Spec-Driven Development

- [ ] New specs use `docs/specs/backlog`, `docs/specs/development`, or `docs/specs/done`.
- [ ] Active work has a spec in `development`.
- [ ] Done work has acceptance criteria and validation results.
- [ ] Legacy spec paths are documented if still in use.

## Agent Context

- [ ] Project records the adopted workspace docs version.
- [ ] Generated content uses `AGENT-CONTEXT` markers.
- [ ] Manual project rules are outside generated blocks.
- [ ] Static docs are not overwritten by automation.

## Memory

- [ ] `agents/memory/README.md` exists or memory is explicitly not adopted.
- [ ] Memory entries use source, confidence, and review metadata.
- [ ] Memory does not contain secrets or transient scratch notes.
- [ ] Obsolete memory is marked superseded instead of deleted.

## Validation

- [ ] Project quality gates are documented.
- [ ] Missing gates are documented as gaps.
- [ ] Repository content does not disclose machine-local or unrelated-project
  information.
