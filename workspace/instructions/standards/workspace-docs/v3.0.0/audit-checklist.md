# Workspace Docs Audit Checklist

Use this checklist to compare a project against `workspace-docs@3.0.0`.

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
- [ ] Development specs contain `Memory Impact` with `Status: pending` until
  the task's durable outcome is known.
- [ ] Done specs resolve memory impact to `updated` or `none` with a rationale.
- [ ] Done specs classified as `updated` identify a category memory file and
  `workspace/agents/memory/changelog.md`.
- [ ] Legacy spec paths are documented if still in use.

## Agent Context

- [ ] Project records the adopted workspace docs version (`workspace-docs@3.0.0`).
- [ ] Generated content uses `AGENT-CONTEXT` markers.
- [ ] Manual project rules are outside generated blocks.
- [ ] Static docs are not overwritten by automation.

## Memory

- [ ] `workspace/agents/memory/README.md` exists or memory is explicitly not adopted.
- [ ] Memory entries use source, confidence, and review metadata.
- [ ] Memory does not contain secrets or transient scratch notes.
- [ ] Obsolete memory is marked superseded instead of deleted.
- [ ] Every completed task handoff states `Memory impact: updated` or
  `Memory impact: none` with a rationale.
- [ ] An `updated` classification has a matching durable entry in one category
  file and a corresponding append-only changelog record.

## Validation

- [ ] Project quality gates are documented.
- [ ] Missing gates are documented as gaps.
- [ ] Repository content does not disclose machine-local or sibling-project
  information.
