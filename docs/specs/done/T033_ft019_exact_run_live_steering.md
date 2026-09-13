# T033: FT-019 Exact-Run Live Steering

**Status:** Done

**Related:**
[FT-019: Codex-Inspired Chat and TUI Interactions](../done/ft_019_codex_inspired_chat_and_tui_interactions.md),
[T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T032: FT-019 Explicit Compaction and Session Background Commands](T032_ft019_explicit_compaction_and_session_background_commands.md), and
[T003: Context Engine with Dynamic Compression](../done/T003_context_engine_with_dynamic_compression_and_session_management.md)

## Summary

Add a presentation-neutral steering channel that is bound to one admitted foreground
agent run. A steering instruction is durably recorded as user-authored exact-run input
before the running agent can observe it. The agent applies accepted input only at a
defined safe boundary: before a provider request, after an in-flight provider response
but before its proposed tool batch is persisted or authorized, or after an already
started tool batch before the next provider request.

TUI `Ctrl+S` and the plain `steer:` prefix expose the same contract. Enter continues to
queue the next turn and never steers.

## Scope

- Add a bounded exact-session/exact-run steering handle and receiver to the agent API.
- Persist ordered `steering_input` session evidence before delivery, and reject stale,
  replayed, terminal, reconciling, wrong-session, and wrong-run submissions.
- Incorporate accepted steering into bounded planning/runtime context at the next safe
  model boundary without mutating the original user message or provider continuation.
- Supersede an unapproved plan generated before newly accepted steering and regenerate
  it from the updated bounded context.
- If steering arrives during a provider response, discard that uncommitted response and
  any proposed tool calls before approval/execution, close private continuation state,
  and request a fresh response with the steering context.
- Expose TUI `Ctrl+S` and plain `steer:` submission while a run is active, preserving
  approval/question/modal precedence and queue semantics.
- Treat explicit compaction as non-steerable maintenance: compact workers and the agent
  API must reject exact-run steering without installing or acknowledging a channel.
- Project steering evidence as typed user activity without rendering the private run ID.
- Update user and technical documentation and retain queue-only behavior when a caller
  does not install a steering channel.

## Non-Goals

- Mutating or cancelling a tool call that already began execution before the steering
  checkpoint.
- Rewriting the original turn message, queued follow-up, plan history, or assistant
  output already committed to the session.
- Interrupting a provider HTTP request mid-frame or synthesizing provider-native
  continuation state.
- Allowing steering during approval, question, reconciliation, or terminal states.
- Changing `nib run`, gateway, MCP, or durable scheduled-run automation contracts.

## Acceptance Criteria

- [x] Every steering channel is bound to one canonical profile session directory,
      session ID, and 32-hex run ID; cross-profile, cross-session, stale, or replayed
      handles fail closed.
- [x] A bounded, control-safe steering instruction is persisted with monotonic per-run
      ordering and source before acknowledgement or agent delivery.
- [x] The receiver accepts only evidence already persisted for its exact run and applies
      each instruction at most once in persisted order.
- [x] Steering submitted before a provider request affects that request's bounded
      context; steering arriving during a response prevents its uncommitted tool proposal
      from reaching approval or execution and triggers a fresh bounded request.
- [x] Steering arriving after a tool has started is applied before the next provider
      request and does not claim to cancel the already-started effect.
- [x] Steering received after plan generation but before approval supersedes the
      unapproved plan with auditable evidence and regenerates planning context.
- [x] TUI `Ctrl+S` steers only when a worker and exact steering handle are active; it
      consumes the current draft only after durable acceptance. Enter still queues.
- [x] Plain mode accepts `steer: <text>` during an active run through the same handle,
      while modal approval/question input retains precedence and `queue: <text>` remains
      a durable next-turn action.
- [x] Explicit compaction never installs or exposes a steering handle; plain, TUI, and
      direct API attempts reject it without persisting `steering_input`.
- [x] Steering events render as bounded user activity without exposing private run IDs,
      raw provider state, credentials, or control sequences.
- [x] Cancellation, terminal reconciliation, renderer exit, and channel loss leave no
      accepted-but-unaccounted execution effect; a persisted delivery failure is explicit
      and never silently becomes queued work.
- [x] Deterministic agent, persistence, reducer, TUI, and plain-mode tests cover success,
      stale/wrong-run rejection, ordering/bounds, response/tool suppression, modal
      precedence, queue distinction, cancellation races, and legacy sessions.
- [x] Independent spec-compliance and code-quality/security reviews report no unresolved
      blocking findings.
- [x] `task test:interactive`, `task test:runtime-e2e`, `task check`, `task test`,
      `task check:all-targets`, `task docs:check`, `task coverage`, `task build`,
      `task smoke:interactive`, and `git diff --check` pass on the completion revision.

## Affected Areas

- `src/agent/loop.rs` and `src/agent/mod.rs` — exact-run steering API, persistence
  validation, safe-boundary intake, and bounded context application.
- `src/interactive.rs` — steer parsing/reduction and typed persisted activity projection.
- `src/chat.rs` and `src/console.rs` — single-owner plain input routing for active-run
  steer, queue, approval, and question input.
- `src/tui/mod.rs` — worker steering handle, `Ctrl+S`, draft disposition, and live ledger.
- `src/session/mod.rs` — only additive event evidence; the message schema is unchanged.
- `tests/interactive_cli.rs`, `tests/test_runtime_e2e.rs`, and focused unit tests.
- `docs/user/guide.md`, `docs/tech/architecture.md`, and the FT-019 reconciliation.

## Validation Gates

1. Pure steering persistence tests prove exact-run binding, monotonic ordering, bounds,
   terminal/reconciliation rejection, and send-failure evidence.
2. Scripted Mock provider tests pause an in-flight response, submit steering, and prove
   the obsolete tool proposal is neither persisted nor executed before the replacement
   request receives the accepted instruction.
3. TUI reducer/TestBackend tests distinguish `Ctrl+S` from Enter and preserve modal
   precedence, draft recovery on rejection, and exact active-run matching.
4. Plain broker tests route approval/question responses before `steer:`/`queue:` and
   prove one submitted line has exactly one consumer.
5. Canonical Task, coverage, release, documentation, cross-target, and native terminal
   gates listed in the acceptance criteria.

## Risks and Decisions

- Steering is checkpointed, not a provider-request abort. The UI describes it as
  accepted for the next safe model boundary.
- Already-started tools retain their normal supervision and reconciliation. Steering
  changes later reasoning only.
- Input is stored as an additive event because inserting a second session message into
  an open assistant/tool continuation would violate the role and continuation contract.
- Persistence precedes channel delivery. If delivery loses a terminal race, the event is
  retained with explicit `steering_delivery_failed` evidence and is not replayed after
  restart or converted into a queued follow-up.
- Native key reliability follows FT-019: `Ctrl+S` is the preferred TUI binding and the
  slash/plain `steer:` path is the semantic fallback on adapters that reserve it.

## Implementation Plan

1. Implement and test exact-run persistence/channel ownership and activity projection.
2. Add safe-boundary agent intake and scripted provider regressions.
3. Route TUI and plain active-run input through the shared steering API.
4. Update documentation, run the two-stage review, and reconcile FT-019/T033 only after
   all local and native gates are satisfied.

## Validation Evidence (2026-08-26)

- The focused interactive gate passed 135 tests: 16 exact-run steering, 35 shared
  interaction, 58 TUI, 19 plain/chat, 6 CLI, and 1 installer-contract test.
- The 16-test runtime end-to-end suite, `task check`, `task test`,
  `task check:all-targets`, `task docs:check`, and `git diff --check` passed.
- Runtime line coverage passed at 85.71% (82,695 / 96,482).
- The locked optimized build and `task smoke:interactive` passed. The release smoke
  exercised the built binary through Linux pseudo-terminal and redirected flows,
  including durable steering intake, plan supersession, queue distinction,
  cancellation reconciliation, and terminal restoration.
- Independent spec-compliance and code-quality/security reviews passed after
  reconciliation. The quality/security review verified compact rejection, admission
  fencing, continuation abandonment, exact ownership, modal routing, shutdown
  accounting, redaction, and resource bounds with no unresolved blocking finding.
