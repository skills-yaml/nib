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

Do not update memory for one-off implementation details that are already obvious from code.
