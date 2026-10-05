# Agent Memory Standard

Project memory stores durable context that future agents should know. It is not
scratch space, a task log, or a secret store.

## Location

```text
workspace/agents/memory/
  README.md
  decisions.md
  facts.md
  preferences.md
  open-questions.md
  changelog.md
```

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

## Rules

- Store only durable decisions, facts, preferences, and consequential open
  questions.
- Never store secrets, credentials, personal data, or transient task progress.
- Append replacements and mark superseded entries instead of deleting history.
- Link to relevant project specs or docs when useful.

## Required Impact Classification

Every completed user-directed task or bounded work item classifies memory
impact as `updated` or `none` before final handoff. Internal commands and tool
calls belong to the containing task.

- `updated`: append the durable entry to the appropriate category file and a
  corresponding record to `changelog.md`.
- `none`: state why no new durable context was created.
- `pending`: allowed only in development or test while the durable result is
  genuinely unresolved; forbidden in done specs and final handoffs.

Every non-trivial development and test spec includes:

```md
## Memory Impact

Status: `pending`

Rationale: The durable outcome will be classified before completion.
```

Resolve the status before moving to done. An updated result identifies both the
category file and `workspace/agents/memory/changelog.md`; a none result includes
a concise rationale.
