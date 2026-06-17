# Agent Memory

This directory stores durable project memory for the `nib` repository.

It follows `workspace-docs@1.0.0` from `/home/e/work/projects/agents/docs/standards/workspace-docs/v1.0.0/`.

- Do not store secrets, credentials, tokens, or transient scratch notes.
- Prefer append-only updates.
- Mark obsolete entries as superseded instead of deleting them.

## Files

- `decisions.md`: durable technical or process decisions.
- `facts.md`: stable project facts that are expensive to rediscover.
- `preferences.md`: human or team preferences that affect future work.
- `open-questions.md`: unresolved questions.
- `changelog.md`: append-only log of memory updates.

## Entry Format

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
