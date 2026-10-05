# Spec-Driven SDLC

`workspace-docs@6.0.0` retains the four-stage specification lifecycle and adds
a mandatory isolation and WIP contract for concurrent repository mutation.

## Lifecycle

```text
backlog -> development -> test -> done
```

- Backlog contains accepted work that is not active.
- Development contains active implementation with scope, acceptance criteria,
  validation gates, risks, affected areas, and pending memory impact.
- Test requires confirmed shared-test integration.
- Done requires confirmed production release and resolved memory impact.

Lifecycle state follows confirmed events, never the checked-out branch name or
a WIP status.

## Multi-Agent Isolation

When two or more agents may write concurrently:

1. choose one full base revision and coordination ID;
2. create a dedicated linked worktree for each mutating agent;
3. leave each worktree detached unless the user directly requested branch use
   for the named task;
4. keep the primary checkout free of implementation changes;
5. create peer-owned task and per-agent records under
   `workspace/docs/work/multi-agent/<coordination-id>/`;
6. validate bounded scopes before work begins; shared files are allowed;
7. retain records through handoff and verify no unique work before cleanup.

Read-only work does not require isolation until the agent begins mutation.
File ownership coordinates writes but never expands approval or task scope.

## Version Impact

Every current spec includes the per-component table in
[Spec Versioning](./versioning.md). Reserve before development; apply once at
development start or merge, and require applied versions before test and
released reservations before done. Each agent checks the shared reservation
and published state before claiming a target and again before integration.

## Peer Runtime

No coordinator agent is required. Every peer self-claims a unique task and
creates its own worktree through the coordination skill's atomic JSON board in
the Git common directory.
All peers may edit the same files. Independent review, serialized integration,
and explicitly configured native gates reconcile combined changes. Claims survive crashes;
failed staging worktrees remain recoverable. The runtime supports one machine
and Git common directory; it does not publish to test or production branches.

## Validation Sequence

Use affected focused gates during iteration, generate affected contracts before
their checks, then complete review and reconciliation before freezing the final
candidate. Run every required aggregate gate on that candidate. A changed
relevant input invalidates its evidence. Gate-specific reuse requires a tested
native dependency map and receipt; unknown scope or environment reruns the
aggregate gate. Integration validates the actual combined revision separately.
