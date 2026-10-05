# Spec-Driven SDLC

`workspace-docs@2.0.0` requires spec-driven development for non-trivial work.

## Spec States

Each spec must be in exactly one state.

```text
specs/
  backlog/
  development/
  done/
```

Legacy spec locations such as `specs/` or `.kiro/specs/` may exist during migration, but new workspace-aligned work should use `specs/`.

## Backlog

`backlog` contains accepted ideas that are not actively being implemented.

A backlog spec should include:

- problem or opportunity
- user or maintainer value
- rough scope
- known constraints
- open questions

## Development

`development` contains active work.

A development spec must include:

- scope
- acceptance criteria
- affected areas
- implementation plan
- validation gates
- risks or rollback notes when relevant

Agents must not start implementation for non-trivial work until the spec is in `development` or the user explicitly authorizes a small direct change.

## Done

`done` contains completed work.

A spec may move to `done` only when:

- implementation is complete
- acceptance criteria match final behavior
- required tests or checks pass
- skipped checks are documented
- docs, generated agent context, or memory are updated if required

## Transitions

Allowed transitions:

```text
backlog -> development
development -> done
```

Exceptional transitions require a note in the spec or project memory:

- `development -> backlog` when work is paused or descoped
- `done -> development` when a regression reopens the work

## Completion Rule

Agents must not declare work complete only because code was changed. Work is complete when the spec, implementation, validation, and durable memory are reconciled.

