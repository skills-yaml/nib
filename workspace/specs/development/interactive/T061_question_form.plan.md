# T061 Implementation Plan

Owner: [T061](T061_question_form.md)
State: development

## Ordered Work

1. Preparation: inspect published baselines and shared reservations; reserve
   nib-question-form 0.3.0, apply once, preserve old documentation membership through linked T067,
   resolve the spec memory to pending, and register AC-1 through AC-9 with explicit
   repository-native validation commands. Independent review validates version controls.
2. Foundation: add normalized form/answer types and semantic validation, keep
   legacy handler entrypoints working, persist linked per-question obligations,
   support exact reuse and discussed results, enforce completion/dependency gating.
3. Recovery API integration: expose typed recovery/editor effects and exact
   lease-fenced persistence before the frontends consume them. Keep the old slash
   commands only on the internal integration ref until frontend fixtures migrate.
4. Surfaces: implement a shared form reducer and TUI card/tabs/editors; adapt
   plain/chat/console/one-shot line protocols with one submit boundary for sets.
5. Recovery integration: route ordinary conversation to exact persisted operation identities;
   reopen unfinished forms before resuming, block ambiguous/unrelated responses,
   preserve existing execution admission and proposal approval requirements.
6. Reconcile: remove /plan and /questions from registration/help/completion,
   update runtime prompt and user guide, add behavioral/regression fixtures for
   each criterion, and resolve durable memory before completion.
7. Review spec compliance, then quality/security/data-integrity/public-interface
   behavior on exact candidates. Fix all findings and renew affected gate modules.
8. Freeze combined implementation and run task check, task test, task docs:check,
   task test:interactive, and applicable native interactive/release qualification
   modules. Register task check/test exactly as Taskfile verify composition.
9. Deliver to development after green exact combined CI; record the actual event
   and move to test. Qualify exact main candidate on Linux/macOS/Windows, verify
   actual main merge, reconcile acceptance/catalog/versions/memory and move to done.
   Renew review/frozen gates for final records before delivery. Publication separate.

## Bounded Lanes

- Contract/runtime: src/agent, src/tools, src/session, shared question_form module;
  schema, invocation identity, exact persistence/reuse, dependency gating, prompt.
- TUI: src/tui, shared question_state module; typed form transport, card drawing,
  tabs/drafts/Submit, editors, cancellation and exact continuation handoff.
- Line adapters: src/chat, src/console.rs, src/run.rs, related binary/CLI tests;
  common rows, literal text, chat editor, Esc/EOF, set submit, recovery handoff.
- Recovery/docs: shared interactive recovery/registration/help modules, user docs,
  acceptance records and durable memory; integrate exact native evidence.

Each mutating peer claims its own task/worktree and owns its WIP record. Cargo
runs are serialized and use a target tied to the exact checkout, preventing
questionable compiled manifest-root reuse. No paid live LLM qualification is needed.

## Freshness and Completion

Frozen final aggregate checks establish exact combined behavior. Native focused
interaction/agent/session/tool modules run during coherent changes and review fixes.
Changed source inputs renew native artifact qualification; documentation-only event
records renew documentation/version/review gates and final aggregate evidence when
freshness is unknown. Preserve unrelated skill package-pin edits in the primary.

## Scope and Acceptance Criteria

This plan implements the complete approved T061 scope and acceptance criteria,
with fresh runtime membership owned by linked T067; no additional behavior is added.

## Affected Areas

The bounded lanes above cover all T061/T067 affected areas and compatibility.

## Validation Gates

Use the focused and frozen Task gates specified above and in T061/T067.

## Risks

Discussion must never resolve dependency obligations; recovery must not guess
operation identity. Preserve old session fields and published version history.


## Integration Slices

The recovery API lane contributes the prerequisites for AC-7 while the final
command-reconciliation task removes registration, parsing, help, completion and
CLI assertions after the migrated native surfaces are integrated. Compatibility
fixtures select an enduring registered status command and native prompt readiness
marker. Every internal integration slice runs the registered native check/test
gates; only the complete accepted candidate is promoted to development and main.
