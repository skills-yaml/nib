# Agent Memory Standard

`workspace-docs@6.0.0` stores durable project context under
`workspace/agents/memory/` and keeps transient operational coordination under
`workspace/docs/work/multi-agent/`.

## Durable Memory

Use memory only for stable decisions, facts, recurring preferences, and
consequential open questions. Append changed entries to the matching category
file and append the corresponding event to `changelog.md`.

Every completed task classifies memory impact as `updated` or `none` with a
rationale. Development and test specs may remain `pending` only while the
durable result is genuinely unresolved; done specs may not remain pending.

## Coordination Records Are Not Memory

Per-agent assignments, action checkpoints, validation status, blockers, task
refs, and handoff notes remain work records. Do not copy routine WIP history
into durable memory. Promote only a stable, non-obvious result through the
normal memory classification and changelog process.

Never store secrets, credentials, personal data, absolute machine paths,
usernames, hostnames, or raw command output in either location.
