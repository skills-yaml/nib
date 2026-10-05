# Workspace Docs Audit Checklist

Use this checklist to compare a project against `workspace-docs@2.1.0`.

## Required Files

- [ ] `AGENTS.md` exists at repository root.
- [ ] `DESIGN.md` exists at repository root.
- [ ] `README.md` exists at repository root.
- [ ] `workspace/instructions/tech/task.md` exists.
- [ ] `workspace/instructions/tech/sdlc.md` exists.
- [ ] `workspace/instructions/tech/project_structure.md` exists.
- [ ] `workspace/instructions/agents/` exists.
- [ ] `workspace/instructions/skills/` exists.
- [ ] `workspace/instructions/standards/` exists.

## Conditional Files & Context

- [ ] Python backend projects have `workspace/instructions/tech/backend_python.md`.
- [ ] CI-enabled projects have `workspace/instructions/tech/ci.md`.
- [ ] Next.js projects have `workspace/instructions/tech/frontend_nextjs.md`.
- [ ] Flutter projects have `workspace/instructions/tech/frontend_flutter.md`.
- [ ] Infrastructure projects have `workspace/instructions/tech/infrastructure.md`.
- [ ] Business/company reference material is stored under `workspace/company/`,
  or that directory is explicitly documented as reserved/not adopted.

## Spec-Driven Development

- [ ] New specs use `workspace/specs/backlog`, `workspace/specs/development`, or `workspace/specs/done`.
- [ ] Active work has a spec in `workspace/specs/development`.
- [ ] Done work has acceptance criteria and validation results.
- [ ] Legacy spec paths are documented if still in use.

## Agent Context

- [ ] Project records the adopted workspace docs version (`workspace-docs@2.1.0`).
- [ ] Generated content uses `AGENT-CONTEXT` markers.
- [ ] Manual project rules are outside generated blocks.
- [ ] Static docs are not overwritten by automation.

## Memory

- [ ] `workspace/agents/memory/README.md` exists or memory is explicitly not adopted.
- [ ] Memory entries use source, confidence, and review metadata.
- [ ] Memory does not contain secrets or transient scratch notes.
- [ ] Obsolete memory is marked superseded instead of deleted.

## Validation

- [ ] Project quality gates are documented.
- [ ] Missing gates are documented as gaps.
- [ ] Repository content does not disclose machine-local or sibling-project
  information.
