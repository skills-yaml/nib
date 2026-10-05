# Workspace Agent Process

This process applies to projects that adopt `workspace-docs@1.2.0`.

## Intake

1. Read the project `AGENTS.md`.
2. Read the relevant required docs under `docs/tech/`.
3. Check the relevant spec state under `docs/specs/`.
4. Check `agents/memory/` for durable decisions or facts that affect the task.
5. Ask only for blocking clarification. Otherwise proceed with a narrow plan.

## Planning

The plan must identify:

- affected modules and files
- related specs
- expected tests or validation commands
- security, auth, data-flow, or infrastructure risk
- docs or memory updates that may be required

## Implementation

Implementation must stay inside the approved scope.

Agents must:

- follow the closest local pattern
- keep diffs focused
- avoid unrelated refactors
- avoid overwriting static docs
- keep generated content inside generated markers

## Verification

Before work is complete:

- run the project quality gates when available
- explain any skipped gate
- confirm acceptance criteria are met
- update the spec state if the task changes state
- update memory only for durable facts or decisions

## Handoff

The final handoff must include:

- what changed
- where it changed
- what validation ran
- what remains open, if anything

