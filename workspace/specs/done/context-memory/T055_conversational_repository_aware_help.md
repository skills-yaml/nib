# T055: Conversational Repository-Aware Help

**Status:** Done

State: done
Primary Feature: context-memory

**Related:** [T050](../interactive/T050_plan_free_answers_decision_prompts_and_run_outcomes.md),
[T028](../interactive/T028_current_session_first_tui_and_slash_command_completion.md).

## Summary

Treat ordinary help questions as user messages and answer them conversationally with
current repository and nib capability context. Keep `/help` as the immediate command
reference.

## Problem

The shared interaction reducer converts a few exact phrases, including `help`, into
the `/help` command. A user asking for help therefore receives a command dump rather
than an answer about what nib can do in the current repository. The conversion also
bypasses the answer-only route introduced by T050.

## Goals

- Plain `help` and capability questions enter the normal interactive agent route and
  can receive one answer without a plan or executable tool.
- A general help answer can name the current repository, relevant work, common
  validation tasks, and a short selection of real interactive commands.
- `/help` remains fast, deterministic, available without a provider, and derived from
  the shared command registry in both plain and TUI presentations.
- Help content comes from bounded local project files and live command metadata;
  absent or malformed sources are omitted rather than guessed.

## Non-Goals

- Changing slash-command names, parsing, completion, or command effects.
- Guaranteeing a fixed wording or section count in model-authored answers.
- Executing a repository task or changing state merely because the user asks for help.
- Changing one-shot `nib run`, explicit plan mode, or the configured answer-only
  override.

## Design

Remove the exact natural-language-to-`InteractiveCommand::Help` conversion from the
shared reducer. Non-slash text follows the existing idle-turn or running-turn queue
path; slash commands continue through the registry parser. Thus `help me refactor`
retains its task meaning, and plain `help` is persisted and reconciled as a normal
user request.

Give the answer-only request a small, optional help snapshot. It contains a bounded
README introduction, descriptions of existing common Taskfile tasks, and selected
available slash commands generated from `INTERACTIVE_COMMANDS`. Read project files
through the existing scoped, no-symlink bounded reader. Limit the snapshot against
the model input allowance, and shed it before required instructions, history, and
the routing control if the request is tight. Treat file content as evidence, never
instructions. The model should use this material to answer general help requests
with a concise project-oriented overview, a few relevant tasks and commands, and
an invitation to describe the work. Specific questions should receive specific
answers rather than the full overview. If evidence is missing, it should say so or
use the existing `request_plan` control.

## Affected Areas

- `src/interactive/`: remove natural-language command substitution; keep `/help`.
- `src/context/` and `src/agent/instructions.rs`: build and budget the help snapshot
  for answer-only requests and guide its use.
- Interactive and answer-route tests, `README.md`, `workspace/docs/user/guide.md`, and T050
  supersession note.

The session, plan, and tool permission models do not change. An ordinary help answer
uses the existing answer-only audit and reconciliation path.

## Alternatives Considered

- Improve the static `/help` string: still makes a plain message act like a command
  and cannot answer a specific question naturally.
- Add a separate intent-classifier model call: unnecessary latency and an additional
  routing decision for behavior the existing answer-only path supports.

## Acceptance Criteria

- [x] `help`, `what can you do?`, and `help me refactor` are ordinary user text in
  idle mode; while a run is active, they follow the existing next-turn queue rule.
- [x] `/help` continues to return registry help without a model request or plan.
- [x] The answer-only prompt includes a bounded snapshot of real project and command
  information when available; missing, oversized, linked, or malformed project files
  do not produce invented task entries or bypass prompt bounds.
- [x] A credential-free answer-route fixture proves a plain help request takes one
  generation, persists one answer, and creates no plan or executable tool record.
- [x] README and user guide describe the distinct plain-language and slash behavior.
- [x] `task check`, `task test:interactive`, `task test:runtime-e2e`,
  `task docs:check`, and `task verify` pass.

## Implementation Plan

1. Remove the reducer substitution and update its behavioral tests.
2. Add the bounded, source-backed help snapshot and answer-only guidance.
3. Add prompt and runtime fixture coverage, then update user-facing docs.
4. Review spec compliance and code quality; run the focused and full gates.

## Validation Gates

- `task check` and `task test:interactive` for input routing and slash parity.
- `task test:runtime-e2e` for the answer-only request and durable outcome.
- `task docs:check` for the spec and user documentation.
- `task verify` before completion.

## Risks and Mitigations

- Additional help context costs tokens. Bound it independently and drop it first when
  the input allowance is tight.
- Model answers can misstate capabilities. Supply only validated task and command
  metadata, instruct the model to use evidence, and retain `/help` as the exact list.
- Help during an active run becomes queued text. Document the existing queue behavior
  and keep `/help` available for immediate command discovery.

## Rollout

No persisted data or configuration migration is required. This spec supersedes only
T050's local command-registry response for plain-language capability requests.

## Open Questions

None for this scope.

## Validation (2026-09-28)

- `task check`, `task test:interactive`, `task test:runtime-e2e`,
  `task test:agent-context`, `task docs:check`, and `task verify` passed.
- The answer-route fixture observed one provider request, one persisted response,
  no plan, and no executable tool record. The request contained current README,
  Taskfile, and command-registry information.
- The context tests covered missing, malformed, oversized, and linked project
  sources and confirmed the bounded prompt stays within its input allowance.
- Spec compliance and quality review found no session-schema, permission, or
  command-parser change. Live model wording remains provider-dependent; no paid
  model call was made.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | historical | Retrospective classification of the preserved pre-v7 outcome; no new bump or release identity is inferred. |

## Memory Impact

Status: none
Rationale: This historical outcome is preserved; migration adds no new durable decision for this spec. Existing dated memory evidence remains authoritative.
