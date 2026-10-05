# Workspace Docs Audit Checklist

Use this checklist to compare a project against `workspace-docs@2.0.0`.

## Required Files

- [ ] `AGENTS.md` exists.
- [ ] `README.md` exists.
- [ ] `instructions/tech/task.md` exists.
- [ ] `instructions/tech/sdlc.md` exists.
- [ ] `instructions/tech/project_structure.md` exists.

## Conditional Files

- [ ] Python backend projects have `instructions/tech/backend_python.md`.
- [ ] CI-enabled projects have `instructions/tech/ci.md`.
- [ ] Next.js projects have `instructions/tech/frontend_nextjs.md`.
- [ ] Flutter projects have `instructions/tech/frontend_flutter.md`.
- [ ] Infrastructure projects have `instructions/tech/infrastructure.md`.

## Spec-Driven Development

- [ ] New specs use `specs/backlog`, `specs/development`, or `specs/done`.
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
