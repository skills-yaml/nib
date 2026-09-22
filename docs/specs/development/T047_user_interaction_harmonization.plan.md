# Implementation Plan for T047 User Interaction Harmonization

**Status:** Development
Created: 2026-09-19
Decision revision: 2026-09-19 — integrates review questions Q1–Q20
Parent: [T047 specification](T047_user_interaction_harmonization.md)

## Scope and Execution Rules

Implement the parent contract in three increments. This plan authorizes no immediate
code changes: move both documents to `development/` when implementation is requested,
repair lifecycle links, and keep acceptance criteria unchecked until evidenced.

Reuse existing reducers, handlers, session leases, clarification records, redaction,
and clipboard backends. Do not add a new framework, authority store, remote listener,
optional-question schema, or context-accounting implementation. Preserve unrelated
working-tree changes and T045/T046 work. Keep new functions within the Rust length
gate rather than introducing broad lint suppressions.

Use fresh implementation lanes where useful, with explicit file ownership; work that
touches shared reducer/session types is sequential. Every increment receives a spec
compliance review followed by a quality/security review. Fix findings before advancing.

## Affected Areas

Shared interaction and presentation modules (`src/interactive.rs`, `src/chat.rs`,
`src/console.rs`, `src/tui/`); runtime/session clarification handling; tool approval
projection; `src/run.rs` and CLI exit plumbing in `src/main.rs`; deterministic tests,
native smoke scripts, and affected
user/technical documentation. Each work package below specifies its narrower ownership.
External handlers are compatibility checks, not new protocols.

## Acceptance Criteria

The parent's AC1–AC10 groups and all lettered subcriteria are authoritative.
Its [review-question decisions](T047_user_interaction_harmonization.md#review-question-decisions)
resolve Q1–Q20 and supersede the previous retype-original-goal recovery plan.
Work-package exit criteria below map to those
requirements; passing a package does not complete the whole task. Completion also
requires the cross-surface matrix, final validation gates, and recorded native evidence.

## Preparation and Baseline

1. Read AGENTS, required technical docs, the parent, FT-019, T038/T039, T042, and current
   T045/T046 state. Check the current source rather than treating the review revision
   as an immutable implementation baseline.
   Agree T046 ownership before modifying shared native smokes; independent work need
   not wait for all of T046. Coordinate overlapping T043 module edits without taking
   on its broad split or imposing a blanket ordering between the tasks.
2. Inventory existing tests and exact terminal key bindings. Run `task test:interactive`
   and `task docs:check`; record pre-existing failures separately from regressions.
3. Create shared fixture cases for questions, approval decisions, request identity,
   and lifecycle outcomes. Characterize current compatibility boundaries and write
   failing regression tests for numeric free text, consumed invalid questions, custom
   TUI answers, incomplete approval detail, and one-shot truncation.
4. Capture sanitized baseline TUI/plain views with Mock fixtures: question, approval
   summary/details, running inspection, final output, no-color, and narrow terminal.
   No external provider credentials or user session contents belong in artifacts.

Exit: concrete regression cases reproduce the scoped gaps; no unrelated baseline
failure is misreported as fixed. Dependencies with T042/T045/T046 are recorded.

## Increment A: Questions and Informed Approvals

### A1. Shared question contract and adapters

- Extend presentation-neutral question request/reduction types and introduce typed
  terminal outcomes. Keep invalid input inside the reducer/renderer retry loop.
- Implement no-options numeric text, one-based options, custom text, `text:` literal
  escape, and explicit empty-input validation. Use the same parser from plain chat,
  console one-shot handlers, and TUI; remove diverging per-surface parsing.
- Keep legacy `QuestionHandler::ask` and add typed `ask_with_context` with a default
  adapter. Map legacy Ok to resolved literal text and Err to InputUnavailable;
  built-ins implement typed outcomes and the runtime calls only that entrypoint.
  Test legacy-only implementers, no recursive delegation, cancellation precedence,
  and late-response rejection. Do not add transport fields or parse error strings.
  Legacy Ok and typed Answered receive only content bounds/non-empty validation;
  never reparse option numbers or prefixes. Test numeric option labels and escaped
  control-looking answers through adapters, persistence, and recovery.
- Implement the parent's precise prefix grammar, including case, required ASCII
  space, bare/malformed/empty payloads, single-pass nested text escapes, and unsupported
  one-shot command entry. Preserve payload content and existing bounds.
- Fix ownership so validation cannot take/drop a pending responder. Ensure exactly
  one terminal outcome and reject events from another run or invocation. Preserve
  plain modal-frame delimiter ownership across validation retries; add delayed
  surplus-input tests instead of relying on draining only currently buffered lines.

Files: `src/interactive.rs`, `src/console.rs`, `src/chat.rs`, `src/tui/`,
`src/agent/loop.rs`, relevant handler tests.
Acceptance: AC1 and the typed-outcome portion of AC3.

### A2. Editable questions and durable recovery

- Reuse bounded composer editing/paste facilities inside the question card. Provide
  explicit option navigation and custom text without preselecting an answer. Digits
  type into the editor and submit only on Enter; Y is text, not an option shortcut.
  Tab/Shift+Tab remain inside the modal; keys cannot leak to the transcript/composer.
- Keep question context and error visible; implement Leave unanswered, cancel, and
  EOF transitions with existing run reconciliation and dependency blocking. Escape
  must release the worker/lease through reconciliation to idle waiting-for-input;
  prove immediate recovery in the same session, not only restart recovery.
- Extend clarification metadata only where necessary, with serde defaults and legacy
  fixtures. Preserve event provenance and answer reuse only within explicitly tested
  eligible scope; do not mark matching text in unrelated records answered.
- Register `/questions [id]`, add readable selectors and startup guidance, and implement
  idle lease-protected exact-record answering with persist-before-ack. Answering does
  not execute or queue work. Add idle-only `/continue <plan-id>` and its matching
  TUI action, with a sanitized summary and explicit execution intent.
- Implement typed existing-plan continuation, distinct from a fresh chat goal.
  Atomically admit against the existing run lease, re-read the exact plan, retrieve
  its original goal internally, audit continuation intent, and bind expected plan ID
  through the new run's admission. Do not synthesize a model-authored goal or add
  a public remote/one-shot protocol. Fresh answer-only behavior remains unchanged;
  continuation uses existing execution subject to all policy/planning/verification
  gates. Reject stale, foreign, completed, genuinely ineligible, malformed, or concurrently owned
  plans without replanning. Test that resolved question-only dependencies can proceed
  despite historical Blocked labels, both alone and with an unrun required check.
  Outstanding verification remains executable work, not an automatic admission failure;
  failed verification uses existing corrective-work rules without waiver. Unresolved
  authority/uncertain-effect blockers still fail closed. Reject missing goal/provenance instead of reconstructing
  display text, and audit Continue against original goal provenance rather than a
  fabricated new user goal. Preserve existing uncertain-side-effect reconciliation.
- Treat a tool-free response with unresolved required verification as a rejected
  completion attempt while normal run bounds permit recovery. Persist the rejection,
  discard the claimed final text, inject runtime-authored corrective context with the
  exact unresolved IDs, and return to the same step. Do not mark the obligation passed, reset
  its evidence, or manufacture human intent. Cover pending and failed checks, successful
  exact corrective execution, repeated premature completion, and exhausted bounds.
- Re-read the record under the lease and reject stale plan/identity/already-answered
  cases. Test failure before persistence, after persistence/before acknowledgement,
  restart, duplicate submission, and a concurrent owner. Extend the fixture through
  explicit plan-ID continuation: same plan ID, recovered answer applied to its exact
  dependency, dependent tool executed once. Cover normal and answer-only configuration,
  changed-plan rejection, missing IDs, and redacted/truncated goal summaries with no
  manual retyping. Verify the existing one-shot same-goal route separately as
  compatibility coverage, not as the required recovery experience.
- Add one-shot guidance to recover via `nib --session <id>` and `/questions`.

Files: A1 files plus `src/session/`, `src/run.rs`, session/runtime fixtures and guide.
Acceptance: AC2, AC3, AC4a–d; no unintended plan-step completion.

### A3. Approval projection and decision parity

- Expand existing approval detail projection from the exact validated tool invocation:
  full redacted command/location, edit preview/paths, memory value/key/namespace, and
  applicable process/delegation/network scope. Reuse bounded/sanitized render helpers.
- Add paged details to all prompt renderers; show omitted-length notices and fail
  closed where the action cannot be represented sufficiently for informed approval.
- Share affirmative/negative/invalid parsing, initial Deny focus, details/back behavior,
  and typed cancellation/EOF outcomes. Buffer TUI typed decisions until Enter; test
  that typing `yes` cannot leave `es` in the next consumer. Retain separate
  workspace/destructive scopes.
- Represent approval EOF as denied/InputClosed, with no retry of closed input or
  false cancellation. Apply default-negative/buffered-affirmative/invalid-retry rules
  to startup consent and management/switch confirmations. Test no grant on startup
  denial and no mutation/session switch on management denial; preserve terminal gating.
- Apply the renderer to reachable legacy `approve_plan` adapters without reactivating
  a plan-approval gate. Add compatibility fixtures for both reachable adapters and
  ordinary execute/plan-only flows that must not prompt for plan approval.
- Confirm approved input cannot change between display and execution; changed input
  needs a fresh decision. Keep explicit deny/require-approval rules under `--yes`.
- Update prompt help and permission documentation with this increment.
  Add the parent's dated supersession notes to affected T039/FT-019 sections and
  coordinate T046 prompt/smoke expectations without changing historical evidence.

Files: `src/tools/executor.rs`, relevant tool implementations, A1 presentation files,
permission tests, README/guide/permissions documentation where affected.
Acceptance: AC5, AC10b–c plus the relevant AC10a supersession notes.

Increment gate: `task check`, `task test:interactive`, `task test:runtime-e2e`,
`task test:tui-shutdown`, `task docs:check`. Extend an appropriate Task target if new
focused suites are not already included; do not run ad hoc Cargo commands.

## Increment B: Live Controls and Complete Output

### B1. Command effect policy and modal-safe inspection

- Classify parsed invocations into read-only inspection, exact-target live control,
  idle management, and existing quit behavior. Enable only the parent allowlist:
  bare `/plan` is inspection, `/plan <prompt>` stays idle-only; `/context details`
  is read-only and live/modal `/stop` without an ID returns usage guidance.
- Route inspection using safe committed/projected state without waiting for the
  worker's execution lease. Preserve current `/context` estimate labels until T042
  supplies its richer snapshot; do not implement duplicate accounting infrastructure.
- Add F2 command-only entry for TUI prompts and the plain `:command` escape. Suspend
  and restore prompt/draft ownership; `text:` preserves command-looking answers.
  F2 without a pending prompt is a no-op. Commands stay inside plain modal framing;
  neither inspection nor its completion consumes the answer or frame delimiter.
- Record T042's handoff explicitly: same entry point and presentation ownership,
  current honestly labeled estimate now, richer T042 accounting later.
- Reject nested approvals and unavailable commands clearly. Test inspection while
  each question/approval/details/selector state is active and while a worker finishes.
- Route `/stop <id>` through existing ownership checks; test missing/stale/foreign
  IDs and completion races without confusing it with foreground cancellation.

Acceptance: AC6, AC7c; no prompt decision, model call, or session mutation from inspection.

### B2. Keyboard/control reconciliation

- Remove Ctrl+C copy interception and implicit idle double-press quit. Preserve
  active-run cancel priority, Ctrl+Q bounded quit, Enter send/queue, and Ctrl+S steering.
- Keep the two-press Ctrl+Q 1000ms window. The first press only arms a hint; the
  second initiates bounded shutdown. Timeout, other input/actions, and consumer
  changes disarm it; Ctrl+C cannot confirm it. Test running, idle, and modal cases.
- Wire a one-shot OS-interrupt owner and existing `CancellationSignal` into `src/run.rs`,
  with the needed non-success exit plumbing in `src/main.rs`.
  Await bounded cancellation/reconciliation rather than exiting in the signal handler;
  retain Unix SIGINT status 130 and qualify native Windows interruption behavior.
  Test real interrupts while a question, approval, and tool are active; assert persisted
  cancellation, no unauthorized continuation, and no orphan owned process.
  Windows must prove actual console/ConPTY Ctrl+C delivery against the exact binary,
  with non-success status, durable cancellation, cleanup, and restoration. Do not
  substitute raw-byte injection or N/A when native signal delivery is unsupported.
- Bind Escape by active consumer; details close without authorizing the underlying
  approval, while top-level approval Escape denies and question Escape leaves unresolved.
- Generate applicable footer/help hints from the active state and shared command policy.
- Exercise cancellation during validation/details/inspection and late responder arrival;
  preserve queued drafts, persisted follow-ups, terminal restoration, and exact-run fences.
- Annotate the affected T038/T039/FT-019 key contracts with dated T047 supersession
  notes when this increment ships, preserving earlier checked criteria and evidence.

Acceptance: AC7a–b, AC7d–e; no stale modal, duplicate terminal event, lost queue, or orphan worker.

### B3. One-shot plan and final answer output

- Render authoritative ordered plan steps in plan-only mode without enabling execution.
  Obtain output from reconciled runtime/session results or safe projected events, never
  raw provider deltas; avoid printing the same final answer twice.
- Replace synopsis-only final output with the full retained sanitized answer, preserving
  paragraphs and code. Make genuine safety/storage omissions explicit and actionable.
- Preserve machine outcome tokens, exit statuses, redaction, and existing redirection
  behavior; add fixtures longer than 512 characters and malformed/failed model turns.

Files for B: `src/interactive.rs`, `src/chat.rs`, `src/console.rs`, `src/tui/`,
`src/run.rs`, command/CLI/runtime tests, native smoke scripts and guide.
Acceptance: AC8 and remaining control/output aspects of AC10.

Increment gate: A's tasks plus local `task smoke:interactive` using Mock fixtures.

## Increment C: Presentation, Wording, and Final Qualification

- Apply consistent question/approval hierarchy and readable plain session choices.
  Add textual roles and non-color state/focus indicators; preserve T045 scan-list rows.
- Verify compact and expanded detail layouts at normal and narrow terminal sizes,
  including long words, wide characters, wrapping, scrolling, and visible actions.
- Make `/review` accurately describe diff inspection. Implement truthful `/copy`
  feedback for native success, unacknowledged OSC52, unsupported backends, and failure;
  retain T045 drag-release copying with the same feedback/redaction rules. Never
  emit clipboard escape sequences to redirected output.
- Reconcile README, user guide, permissions docs, command help and shortcut hints.
  Explain changed approval/Enter/Ctrl+C defaults, informational plans, one-shot output,
  unanswered recovery, and external/noninteractive boundaries.
  Include exact-plan Continue, unchanged double Ctrl+Q confirmation, and all entries
  in the parent's supersession map; annotate affected historical specs on shipment.
- Compare sanitized before/after captures for every affected surface. Capture native
  Linux/macOS/Windows evidence; unit render snapshots do not replace input/restoration
  smokes or establish universal screen-reader/clipboard support.

Acceptance: AC9a–c, AC10a–d, and end-to-end regression of every AC1–AC8 subcriterion.

## Cross-Surface Scenario Matrix

Run shared semantic fixtures through TUI/plain/console adapters where the capability
exists. Mark intentional N/A boundaries explicitly rather than treating absence as
test success. Each row includes observable output and authoritative state assertions.

| Scenario | Surfaces | Required assertion |
| --- | --- | --- |
| Free text `42`, `0`, Unicode | TUI/plain/one-shot | Exact intended answer persisted once; no accidental option parsing |
| Multiline bracketed paste; line-oriented input | TUI multiline; plain/one-shot single-line | TUI paste is one editable answer; line-oriented framing and hints remain explicit |
| Options: first/last, custom, `text: 42`, zero/out-of-range/empty | All terminal prompts | Correct answer or local retry; invalid input retains responder and draft |
| Numeric option label and escaped control text through typed/legacy handlers | Input adapters/runtime/session | Parse once; resolved answer text is never treated as another index or command |
| Invalid then valid input | All terminal prompts | One persisted answer, no intervening failed tool result/model call |
| Delayed surplus input around modal frame completion | Plain chat | No leftover line becomes a new goal, command, answer, or approval |
| Leave unanswered, EOF, Ctrl+C, absent handler | Applicable terminals/external adapters | Distinct outcome; dependencies blocked; no implicit queue replay |
| Restart then `/questions`, wrong ID/plan, duplicate/concurrent answer | TUI/plain plus session/runtime | Exact record only, durable acknowledgement, no automatic execution |
| Recovered answer then exact-plan Continue, answer-only on/off | TUI/plain plus runtime | Same plan resumes dependency once, without retyping a redacted goal or relaxing policy |
| Missing/stale/foreign/completed/ineligible/concurrently owned continuation ID | TUI/plain plus runtime | Fail closed by actual blocking cause; no replacement plan, queue replay, or unrelated answer consumption |
| Answered clarification with historical Blocked step and unrun verification | TUI/plain plus runtime | Continue can run required work/checks; obligations remain enforced and no uncertain side effect is replayed |
| Premature completion with pending/failed required verification | Runtime | Completion text is discarded; exact IDs reach one bounded corrective turn; audited success can complete the same plan |
| Existing same-goal one-shot execution with answer-only on | One-shot plus runtime | Existing routing remains compatible; not the required recovery UI |
| Approve/deny/invalid/empty/details/back | All approval prompts | Same affirmative contract, default denial, inspection never approves |
| Consent/management/switch EOF and explicit denial | TUI/plain where applicable | No grant/mutation/switch; existing startup gating and authority preserved |
| Legacy typed-handler default and approve_plan adapter | Runtime/adapter fixtures | Conservative compatibility, no error-string parsing or revived plan approval gate |
| Long command, patch, memory value, secrets/control bytes | All approval prompts | Full inspectable redacted identity; no injection or raw-secret persistence |
| Invocation replaced after display; response after cancel | Reducer/executor plus adapters | Stale decision rejected, no unauthorized tool call |
| Live inspection and `/stop`; command-looking literal answer | TUI/plain | Allowed effects only, original draft retained, exact cancellation target |
| Enter/Ctrl+S/Ctrl+C/Ctrl+Q with modal/selection/race | TUI; supported plain equivalents | Correct route, queue durability, bounded reconciliation/restoration |
| Ctrl+Q first/second press, timeout, other key, consumer change | TUI | Exact 1000ms confirmation; no first-press cancellation or Ctrl+C confirmation |
| Prefix case/separator/empty/nesting; idle F2 | Applicable prompt adapters/TUI | Single-pass grammar, local errors, retained frame/draft; no idle F2 action |
| OS interrupt during question, approval, and tool | One-shot native terminal | Non-successful exit, durable cancellation, bounded cleanup, no abrupt abandonment |
| Plan-only, long final answer, failure/partial response | One-shot and output helpers | No execution in plan mode, no synopsis truncation or unvalidated output |
| NO_COLOR/narrow/Unicode/focus; session list | TUI/plain | Roles/actions/status readable without hue or clipped-only controls |
| Native clipboard success/failure, OSC52, redirected pipe | Supported terminals/plain/redirected | Truthful result; fallback labeled; no OSC52 on pipes |
| Explicit deny/require approval with `--yes`, MCP/gateway missing handler | Runtime/external adapters | Existing policy and transport schemas remain authoritative |

## Validation Gates and Completion Evidence

Documentation-only authoring uses `task docs:check`, source review, whitespace checks,
and two-stage review. Implementation completion requires:

1. All focused increment gates pass and every AC has a linked test/evidence result.
2. `task verify` passes once on the final local revision; CI coverage and ordinary
   native checks pass. Record revision, exact commands, outcomes, and exceptions.
3. Linux/macOS `task smoke:interactive` and Windows
   `task smoke:interactive:windows:binary` pass for exact built binaries, including
   redirected input, real terminal input, prompt races, and terminal restoration.
4. Spec-compliance review then quality/security review find no unresolved blocker.
5. Docs and persisted-format compatibility fixtures agree with shipped behavior.
   No paid live qualification is required or implicitly authorized.
6. Move parent and companion to `done/` only after all criteria pass; repair index and
   lifecycle links, record evidence, and leave no unchecked acceptance items there.

## Risks and Stop Conditions

Stop the affected implementation increment for an unresolved authority/lease conflict,
uninspectable approval, secret leak, stale-answer acceptance, new external protocol
requirement, or conflict with another active spec's ownership. Record the blocker in
development and narrow/resolve it before proceeding; do not bypass a gate or mark an
unrun native platform complete. Small helper extraction is allowed; broad module
reorganization or context-budget work needs its own owning spec.

## Authoring Validation (2026-09-19)

- Spec-compliance and quality/authority reviews passed; this is proposal review,
  not evidence that the proposed interaction behavior is implemented. That initial
  review preceded the 20-question independent review and is not an unconditional
  readiness claim for this revision. The user accepted the recommended resolutions;
  this revision integrates them in both documents. Fresh spec-compliance and
  quality/authority review passed after clarifying continuation eligibility by blocking
  cause and single-pass answer parsing; both have explicit regression requirements.
- `task docs:check` passed all five checks; whitespace review was clean.
- `task verify` passed its static phase, then its sandboxed test phase failed on
  68 localhost-listener permission errors. The permitted unrestricted `task test`
  rerun passed 1,161 library tests, 86 CLI tests, and the intervening integration
  suites, but stopped at four `session_roundtrip` helper-spawn failures.
- The four failures were `child_process_rejects_session_file_replacement_during_update`,
  `child_process_rejects_whole_session_directory_replacement`,
  `child_process_updates_do_not_lose_session_events_or_memory`, and
  `killed_child_releases_session_lock`. Each reported OS error 2 (`No such file or
  directory`) at the child spawn in `tests/session_roundtrip.rs`; the cause was not
  established. Later suites were not reached, so the complete local gate is not green.
- No application code was changed to address this unrelated verification issue.
  Resolve or reproduce that baseline issue before implementation acceptance; all
  parent acceptance criteria and native implementation qualification remain open.
- The recommendation-integration revision reruns documentation integrity and whitespace
  checks only; it does not claim a new full-runtime or native qualification result.

## Implementation Checkpoint (2026-09-21)

- Implemented the remaining source-audit gaps in durable question recovery, exact-plan
  continuation, approval detail inspection, Ctrl+Q disarming, one-shot final output,
  truthful clipboard behavior, and native one-shot interruption handling.
- Added deterministic recovery/continuation, TUI/plain interaction, redirected-copy,
  and real Unix SIGINT fixtures. Extended the Unix PTY and Windows ConPTY release
  scripts for interruption during a question, approval, and foreground tool.
- Passed `task check`, `task test:interactive`, `task test:agent-context`,
  `task test:runtime-e2e`, `task test:tui-shutdown`, `task docs:check`,
  `git diff --check`, the complete local `task verify`, `task coverage` at 85.78%
  (113,996/132,896 lines), and the final Linux optimized sequence `task build` plus
  `task smoke:interactive:binary`. The successful full gate ran 1,174 library tests,
  89 binary tests, and every ordinary integration target; only
  the repository's explicitly opt-in paid/live and release qualification tests were
  ignored under their existing contracts. Sanitized Linux evidence under
  `target/t047-local-evidence/Linux/` retains F2/question, clipboard,
  interruption, privacy-scan, and before/after terminal-mode results for this working
  tree, with source/binary identity recorded and acceptance eligibility false because
  the source tree is dirty. The smoke-driven review also closed a plain resumed-history
  privacy gap by routing it through the shared public conversation projection.
- Local Windows qualification could not start because `pwsh` is not installed; macOS
  was not available on this Linux host. Native clipboard qualification and a recorded
  exact revision also remain open. The parent and companion therefore stay in
  `development/`; their unchecked native/completion criteria are not waived.
