# Agent Memory Standard

Project memory stores durable context that future agents should know.

Memory is not scratch space, a task log, or a secret store.

## Location

Canonical location:

```text
workspace/
  agents/
    memory/
      README.md
      decisions.md
      facts.md
      preferences.md
      open-questions.md
      changelog.md
```

## Files

- `README.md`: explains how project memory is maintained.
- `decisions.md`: durable technical or process decisions.
- `facts.md`: stable project facts that are expensive to rediscover.
- `preferences.md`: human or team preferences that affect implementation.
- `open-questions.md`: unresolved questions that should be revisited.
- `changelog.md`: append-only record of memory updates.

## Entry Format

Use this format for memory entries:

```md
## YYYY-MM-DD - Short Title

- Type: decision | fact | preference | open-question
- Source: user | repo | spec | command | review
- Confidence: high | medium | low
- Review: YYYY-MM-DD or none
- Supersedes: none or entry title/date

Content:

Brief durable note.
```

## Rules

- Store only durable context.
- Do not store secrets, tokens, private keys, credentials, or personal data.
- Prefer appending a new entry over rewriting old entries.
- Mark obsolete entries as superseded instead of deleting them.
- Link to specs or docs when the memory came from project artifacts.
- Do not record transient task progress; use specs or task trackers for that.

## When To Update

Update memory when:

- the user gives a durable preference
- a recurring project decision is discovered
- a non-obvious setup constraint is confirmed
- a standard migration records an accepted deviation
- an open question blocks or shapes future work

Do not update memory for one-off implementation details that are already
obvious from code.

## Required Impact Classification

Every completed user-directed task or bounded work item must classify its
memory impact before handoff. Internal commands, reads, tool calls, and
incremental implementation steps are part of the task and do not each require a
separate classification.

Use exactly one resolved status:

- `updated`: the task produced new or changed durable context.
- `none`: the task produced no new durable context.

Non-trivial work may use `pending` only while its spec is in `development`.
`pending` is invalid in a done spec or final handoff.

Apply this decision procedure:

1. Did the task establish or change a durable decision, stable non-obvious fact,
   recurring preference, or consequential open question?
2. If no, classify the impact as `none` and state why the result is transient,
   code-obvious, already recorded, or otherwise not durable.
3. If yes, classify the impact as `updated`, append an entry to the appropriate
   category file, and append a corresponding entry to `changelog.md`.
4. If an existing entry is obsolete, append the replacement and identify the
   superseded entry instead of deleting history.
5. State the resolved status and rationale in the final handoff.

## Spec Record

Every non-trivial development spec must include this section:

```md
## Memory Impact

Status: `pending`

Rationale: The durable outcome will be classified before completion.
```

Resolve it before moving the spec to `done/<primary-feature>/` and updating the
root specs catalog. When memory is updated, use:

```md
## Memory Impact

Status: `updated`

Rationale: Brief explanation of the durable context created or changed.

Files:

- `workspace/agents/memory/decisions.md`
- `workspace/agents/memory/changelog.md`
```

Use the appropriate category file; `decisions.md` is only an example. When no
memory update is needed, use:

```md
## Memory Impact

Status: `none`

Rationale: Brief explanation of why the completed task added no durable context.
```
