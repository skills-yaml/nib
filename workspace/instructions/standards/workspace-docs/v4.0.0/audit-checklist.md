# Workspace Docs Audit Checklist

Use this checklist to compare a project against `workspace-docs@4.0.0`.

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
- [ ] `workspace/specs/README.md` exists.

## Conditional Files & Context

- [ ] Python backend projects have `workspace/instructions/tech/backend_python.md`.
- [ ] CI-enabled projects have `workspace/instructions/tech/ci.md`.
- [ ] Next.js projects have `workspace/instructions/tech/frontend_nextjs.md`.
- [ ] Flutter projects have `workspace/instructions/tech/frontend_flutter.md`.
- [ ] Infrastructure projects have `workspace/instructions/tech/infrastructure.md`.
- [ ] Business/company reference material is stored under `workspace/company/`,
  or that directory is explicitly documented as reserved/not adopted.

## Spec-Driven Development

- [ ] Every non-legacy spec uses
  `workspace/specs/<state>/<primary-feature>/<spec>.md`.
- [ ] Every primary feature uses lowercase hyphen-case and is defined in the
  root specs README.
- [ ] Every non-legacy spec has exactly one root catalog row.
- [ ] Every catalog row has a matching link, primary feature, state, and
  non-empty status rationale.
- [ ] Catalog rows are updated with spec creation, transitions, primary-feature
  changes, reopening, supersession, or material status-rationale changes.
- [ ] Active work has a spec in `development/<primary-feature>/`.
- [ ] Done work has acceptance criteria and validation results.
- [ ] Development specs contain `Memory Impact` with `Status: pending` until
  the task's durable outcome is known.
- [ ] Done specs resolve memory impact to `updated` or `none` with a rationale.
- [ ] Done specs classified as `updated` identify a category memory file and
  `workspace/agents/memory/changelog.md`.
- [ ] Legacy spec paths are documented if still in use.

## Agent Context

- [ ] Project records the adopted workspace docs version (`workspace-docs@4.0.0`).
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
- [ ] Spec category and root catalog validation is deterministic and required.
- [ ] Missing gates are documented as gaps.
- [ ] Repository content does not disclose machine-local or sibling-project
  information.
