# T047: User Interaction Harmonization

**Status:** Done
Created: 2026-09-19
Decision revision: 2026-09-19 — integrate the accepted review recommendations
Companion: [Implementation plan](T047_user_interaction_harmonization.plan.md)

## Summary and Decision

Give nib one behavioral contract for questions, approvals, inspection, and recovery,
with native presentations in TUI, plain chat, and one-shot `nib run`. Reuse the shared
interaction reducer, tool executor, and profile-scoped session authority; do not add
a second interaction framework or workload store.

Deliver in three increments: (A) questions and informed approvals, (B) live controls
and complete output, and (C) presentation, terminology, and documentation. This is a
proposal, not a claim that the behavior already ships. Implementation requires moving
this spec and its companion into `development/` and passing their gates.

## Problem and Evidence

The interaction review found semantic differences that presentation alone cannot
justify. Source anchors below were checked against revision `0f2c0c2`:

- [Shared question parsing](../../../src/interactive.rs),
  [plain chat](../../../src/chat.rs), and [console prompts](../../../src/console.rs)
  disagree on numeric free text and local retries. The shared parser treats a numeric
  answer as an option index even when there are no options; plain chat can consume a
  pending question before validation succeeds.
- [TUI question handling](../../../src/tui/mod.rs) accepts option selection but not
  custom text when options exist. Modal editing/paste differs from the composer.
- [Approval context](../../../src/tools/executor.rs) and the console/TUI presentations
  do not consistently expose enough detail to inspect a command, edit, or proposed
  memory value. Console affirmative parsing and TUI initial approval focus differ.
- [Command policy](../../../src/interactive.rs) rejects inspection and `/stop` while
  a worker is active, precisely when monitoring and intervention are useful.
- [One-shot output](../../../src/run.rs) does not attach a plan event consumer and
  reduces the final response to a short flattened summary.
- TUI role distinction relies heavily on color; copy feedback can claim success
  without delivery confirmation. `/review` displays a diff, and `/copy` can display
  text without copying it. Documentation also overstates plan approval behavior.

These findings are source-backed behavioral evidence, not a completed visual or
assistive-technology qualification. Native terminal captures and interaction tests
are required during implementation.

## Goals and Scope

- The same input has the same meaning on every surface offering that capability.
- Invalid input is recoverable locally; cancellation, EOF, and unanswered questions
  remain distinct from validation errors and successful answers.
- Approval requires informed, explicit consent to one exact pending invocation.
- Users can inspect and stop active work without accidentally answering a prompt.
- Plans, final answers, role identity, and recovery actions remain understandable in
  plain, no-color, narrow-terminal, and redirected operation.
- Every acknowledged answer, queued request, cancellation, and completed run remains
  reconcilable against the authoritative session/workload state.

Scope includes interactive startup consent, composer/queue/steering, questions,
approvals, selectors, confirmations, inspection, cancellation, result/error reporting,
clipboard feedback, and their documentation. Existing auth/config/doctor/updater,
durable-task, MCP, and gateway flows are compatibility boundaries, not redesigned
products in this task.

## Non-goals

- New permissions, broader `--yes` authority, a new plan approval gate, or bypassing
  policy, worktree isolation, tool validation, or reconciliation.
- A web UI, new terminal framework, arbitrary module cleanup, new model/provider
  behavior, remote MCP transport, or external channel authentication/listeners.
- Complete context-budget accounting; [T042](../backlog/T042_context_budgeting_and_live_visibility.md)
  owns that data contract. Until available, `/context` must label existing estimates
  honestly rather than claim complete request usage.
- Optional-question/fallback semantics: current `ask_question` requests remain
  required for their declared dependency scope. Do not invent an optional schema
  field or silently continue dependent work when a question is dismissed.
- New one-shot slash commands, a background prompt inbox service, automatic restart
  of interrupted runs, paid provider qualification, or production non-Linux delegation.

## Interaction Inventory and Classification

Four interaction kinds govern rendering and input ownership. Lifecycle operations
such as cancel/quit are controls, not extra questions or implicit approval requests.

| Kind | When and why | How and completion rule |
| --- | --- | --- |
| Information | Startup notices, progress, plans, tool outcomes, status, final answers, failures | Non-blocking display; actionable recovery text where relevant. A plan is informational, including in execute mode. |
| Question | Required task information is missing | Prompt plus editable answer and optional suggestions. Validation retries locally; only a persisted answer satisfies the dependency. |
| Approval | Policy requires consent, workspace access is requested, or a destructive management action needs confirmation | Show the specific action/scope/effects; explicit affirmative decision. Tool, workspace, and management approvals retain separate authority and lifetime. |
| Selection | Choose a session, model, history item, or detail view | Preview and choose; Escape cancels without changing selection. Selecting a session is not authorization to execute work. |

Interaction families and surface boundaries:

| Family | TUI / plain chat | One-shot or external boundary |
| --- | --- | --- |
| Workspace consent and startup notices | Interactive-terminal consent; notices do not steal input | Existing redirected/one-shot startup contract unchanged |
| New task, queued follow-up, live steering | Shared grammar and exact-run routing; presentation differs | `nib run` remains one explicit task, not a chat session |
| Required question and unanswered recovery | Same parser; idle `/questions` recovery | Console question parser matches; saved recovery can use chat |
| Tool approval / destructive confirmation | Same decision semantics, separate authority | Console approval matches; unavailable handlers never imply consent |
| Session/model/history selection | Shared capabilities, TUI picker or readable numbered plain choices | Existing CLI flags/commands retain their own grammar |
| Plan/progress/tool details, final answer, failure | Shared sanitized meanings with native presentation | One-shot prints its authoritative plan/result; stable outcome tokens retained |
| Live inspection and stopping work | Explicit read-only/control command policy | Existing task/CLI controls retain their scope |
| Copy, diff, help, completion, cancellation, quit | Accurate labels, documented keys, no hidden approval | Clipboard unavailable in redirected mode; output fallback is labeled |
| Auth/config/doctor/update, durable work, MCP/gateway | Reuse applicable terminology, preserve existing contracts | No new prompts, remote listeners, or authority expansion |

## Proposed Design

### 1. Shared outcomes and input ownership

Extend the existing presentation-neutral reducers and request adapters with typed
question outcomes: answered, left unanswered, cancelled, input closed, and input
unavailable (for example, an absent handler). Invalid
input is a local validation result, never a terminal question outcome. Do not infer
cancellation by searching error strings. Keep the public legacy
`QuestionHandler::ask(...) -> Result<String, String>` method and add a typed
`ask_with_context(...) -> QuestionOutcome` method with a default implementation.
Its context carries the exact request identity, question, and options. The default
calls legacy `ask`: `Ok` becomes resolved literal answer text; `Err` becomes
`InputUnavailable`, leaving the clarification unresolved without inspecting error text.
Built-in handlers implement the typed method; legacy entrypoints delegate without
recursive defaults. The runtime calls only the typed method. Runtime cancellation
remains a distinct signal and takes precedence over late legacy handler results.
Legacy `Ok` and typed `Answered` already contain resolved text: validate only content
bounds/non-emptiness at this boundary, never parse prefixes or option numbers again.
Number selection and prompt-prefix interpretation happen once in the terminal input
adapter. Selecting an option labeled `42` answers literal `42`; escaped `:command`
or `text:` content remains literal through persistence and recovery.

Only the active consumer receives ordinary input. Inspection temporarily overlays a
question/approval without consuming its responder or losing its draft. Every pending
request and decision is bound to project/profile/session/run and invocation identity;
stale events are rejected after cancellation, session changes, and worker replacement.
No display or clipboard operation can authorize tools.

Preserve plain chat's explicit blank-line modal-frame delimiter after a terminal
decision; local validation retries stay inside the same frame. Delayed surplus lines
inside that frame must not become later goals, commands, answers, or approvals.
One-shot retains its documented line-per-response stream rather than acquiring chat
framing; buffered lines are not replayed as new goals or transferred across runs.

### 2. Questions, editing, and recovery

- After reserved prompt-prefix handling below, with no options every non-empty
  answer, including `0` and `42`, is text.
- With options, a bare positive integer selects the corresponding one-based option.
  Zero/out-of-range integers are local validation errors. Other non-empty text is a
  custom answer. The explicit `text: <value>` form forces a literal answer, including
  numeric text or reserved prompt-control syntax; strip only that prefix.
- TUI exposes suggestions plus a custom-answer editor. Tab moves between the editor,
  suggestions, and actions; arrow keys navigate suggestions only when that list has
  focus. Tab/Shift+Tab cycle within the active modal, not into the transcript. Digit
  keys in the editor insert text; only Enter submits. In other modal controls they
  do not submit an option. `Y` is ordinary answer text, not an alias for option 1.
  Reuse composer cursor, Unicode editing, selection, and bracketed-paste behavior without history submission
  or accidental queue/steer routing. Keep the question visible while editing; long
  questions/options are scrollable. Plain and console prompts show the same choices
  and explain the number-or-text grammar.
- Empty text is invalid unless the user explicitly selected an option. TUI starts
  in the answer editor with no selected answer, so bare Enter does not silently
  choose the first option. In TUI, submitted multiline paste remains one answer.
  Plain/one-shot remain line-oriented: a newline submits a single answer, and their
  hints must not promise multiline editing or TUI paste framing.
- Invalid input retains the pending responder and draft and renders an inline error;
  it emits no failed tool result, terminal run outcome, or extra model request.
- Label dismissal `Leave unanswered`, not `Skip`. Escape leaves a question unresolved
  and reconciles the active worker to idle `waiting_for_user_input`, releasing its
  lease so `/questions` works immediately in the same session; restart is not required.
  Ctrl+C cancels the active run. In an idle recovery editor it only dismisses the editor
  without changing the saved answer/status. EOF records input-closed/unresolved and reconciles
  the run as waiting for input without retry spinning. Cancellation remains cancelled.
  Neither path satisfies dependent actions or silently starts queued work.

Use existing `ClarificationRecord` and lifecycle/human-intent events. Add only
backward-compatible fields needed for typed reason/provenance; missing fields in old
sessions must retain conservative unresolved semantics.

Add idle-only `/questions [id]` to both chat modes. Without an ID, list unresolved
records for the current active plan with readable text, status, and stable invocation
ID; selecting a record opens its answer editor. With an ID, open that exact record.
An idle session-resume notice points to this command when clarification is outstanding.
One-shot waiting-for-input output gives `nib --session <id>` followed by `/questions`;
it does not add an interactive command loop to `nib run`.

Recovery acquires the existing session mutation/run lease, revalidates the record and
plan, and durably records the answer and provenance before acknowledging it. A stale,
foreign-plan, already-answered, or concurrently changed record is rejected without
overwriting it. Answering does not automatically run tools or queued requests.

Add idle-only `/continue <plan-id>` to both chat modes and a separate TUI
`Continue this plan` action that dispatches the same command with the exact stored ID.
After an answer is persisted, show the sanitized plan summary and this explicit
action. Never require the user to reconstruct a truncated/redacted original goal.
There is no implicit current-plan form: a missing ID produces usage guidance.

Continuation is an explicit execution request, not a new chat goal or another
approval. Under the existing run-admission lease, re-read and validate the same
project/profile/session/plan identity, its incomplete resumable state, resolved
clarifications, and reconciled prior run. Reject stale, foreign, completed, malformed,
still-blocked, or concurrently owned plans without generating a replacement plan.
Here still-blocked means an unmet clarification or other execution-blocking prerequisite,
not a historical `Blocked` step label alone: re-evaluate resolved clarification
dependencies under the lease without clearing verification obligations or unrelated
blockers. Outstanding, not-yet-run verification is work for the continuation, not by
itself an admission failure; retain its obligation and require its result before
completion. Failed verification follows existing corrective-work eligibility, never
an implicit pass or waiver. Unresolved authority and uncertain side-effect blockers
still fail closed. Missing persisted goal/provenance is invalid; never reconstruct it from the
display summary. The continuation event references the saved goal's provenance and
records the new explicit Continue intent, not a fabricated freshly typed user goal.
Load the original goal internally from that record; bind the resulting new run to
the expected plan ID through execution admission, and audit the human continuation
intent before acknowledgement. Use existing interruption reconciliation; uncertain
prior side effects are not replayed merely because Continue was selected.

Represent continuation as a typed request kind, distinct from a fresh interactive
goal. `agent.answer_only` continues to apply to fresh chat requests, not this explicit
request to execute an existing plan; continuation uses the existing execution path.
Policy, approval, workspace, configured planning constraints, verification obligations,
and run fencing remain authoritative. Unsupported/non-executable state produces an
actionable refusal, never a silent mode override or replanning operation.

The existing `nib run --session <id> <original-goal>` route remains compatible for
users who already possess the goal, but is not the required recovery UI. Source review
confirms its default `interactive_request = false` excludes it from the interactive
answer-only eligibility check; test actual same-plan recovery, rather than inferring
completion from that routing flag. No new one-shot slash command or CLI flag is added.
The runtime must consume this exact recovered clarification when its dependency is revisited; textual
similarity alone must not resolve unrelated pending questions. Existing intentional
answer reuse needs explicit provenance and tests for its eligible scope. Revalidate
the current plan before recovery and continuation; report changed/non-resumable plans
without silently transferring the answer. Verify actual same-plan dependent execution,
not just successful storage of a recovery answer.

A tool-free model response is a completion attempt, not authority to abandon remaining
required verification. If the current step still has pending, failed, cancelled, or
stale required obligations and the normal turn/transition bounds permit more work,
reject and discard that response, persist the rejected-completion evidence, provide a
runtime-authored corrective context containing the exact unresolved verification IDs,
and return to the same plan step. That context is not human intent and cannot waive or
pass an obligation. A later exact audited invocation remains the only way to satisfy
the check. Repeated premature completion attempts remain bounded; an exhausted bound,
changed plan binding, denied action, or other non-recoverable condition still
reconciles terminally with the plan incomplete.

#### Prompt prefix grammar

Process prefixes before number/option parsing, once, using the case-sensitive ASCII
tokens `text:` and `:command`. Ignore leading spaces/tabs when recognizing a prefix;
require at least one ASCII space after the exact token. A bare token, missing separator,
or whitespace-only payload is a local validation error, not a submitted answer. Remove
the token and one separator space only; preserve remaining payload text, subject to
existing size/sanitization bounds. Case variants are ordinary answer text, not controls.
Do not interpret the payload of `text:` again: `text: text: 1` answers `text: 1`, and
`text: :command /status` answers literal `:command /status`.

`:command <slash-command>` executes only in plain-chat pending prompts. It runs
the allowlisted parsed command inside the current modal frame and restores that
prompt/draft; it does not complete the prompt or consume its blank-line frame delimiter.
`text:` is a question-answer escape on all terminal surfaces, never an approval bypass.
TUI uses F2 for commands; TUI and one-shot reject typed `:command` control syntax as
unsupported and explains the `text:` escape when a literal question answer is intended.

### 3. Informed approvals and safe defaults

Reuse `ApprovalContext` with a common summary: action, target, expected effects,
permission/risk, working location, scope/lifetime, and why approval is required.
Tool approval is labeled `Approve once` / `Deny`; workspace grants and destructive
confirmations describe their actual lifetime instead of inheriting tool language.

Details must expose the complete inspectable, redacted action within existing input
bounds: full command and working directory; affected paths and edit preview; memory
namespace/key and proposed value; delegation/process/network scope where relevant.
Do not silently replace a full command with a 120-character summary. Details can be
paged with an explicit length/omission notice; render controls safely and never reveal
redacted secrets. If the action cannot be represented sufficiently to identify its
effects, fail closed with an actionable explanation rather than approve a summary.
Redaction placeholders alone do not make an otherwise inspectable action unapprovable.

Build details from the same validated invocation the executor will run, not a second
mutable read. Bind consent to that identity and material input; changed input requires
a new decision. Do not persist extra raw arguments, secrets, or clipboard contents.

All terminal surfaces accept case-insensitive `y`/`yes` and `n`/`no`. Enter in the
initial/default state, Escape, and EOF never approve. TUI initially focuses `Deny`;
Enter approves only after deliberately selecting `Approve once` or submitting an
explicit affirmative text response. Typed TUI responses are buffered until Enter,
not accepted on the first `y`/`n` key, so the remainder of `yes`/`no` cannot leak to
another consumer. Invalid text stays in the prompt
with an error. EOF produces a denied decision with typed `InputClosed` reason; do not
retry a closed input or mislabel it as run cancellation. Escape and initial Enter deny
with their own reasons. Ctrl+C cancels the run, not merely the approval. Plain/console expose
`details`; TUI offers a labeled details control. Detail-view Escape returns to the
decision without deciding it.

Apply the same default-negative, buffered affirmative, and invalid-retry rules to
interactive workspace consent and destructive management confirmations. For a session
switch confirmation, default to Cancel and require explicit confirmation; it grants
no execution authority. Workspace denial/EOF leaves no grant and exits the gated
startup path; management/switch denial/EOF leaves existing state/session unchanged.
Tool denial follows the existing denied-tool reconciliation path. Keep grant scope,
workspace identity, terminal-only gating, and redirected startup behavior unchanged.

Any reachable legacy `approve_plan` prompt uses the same renderer and decision rules.
Compatibility coverage must not restore that gate to the normal execute/plan-only
flows, grant new permission, or require a broad removal of legacy adapters.

Explicit deny policies and `require_approval` remain authoritative under `--yes`.
Plans remain informational: plan-only mode prints a plan without executing it; execute
mode uses existing execution policy rather than adding a plan rubber stamp.

### 4. Live commands and keyboard contract

Classify the existing command registry by effect, not one blanket idle requirement:

- Read-only while idle, running, or awaiting a prompt: `/help`, `/status`, `/context`
  (including `details`), bare `/plan`, and `/ps`. Classify the parsed invocation, not
  only its command name: `/plan <prompt>` generates a new plan and remains idle-only.
  Read committed/safe projected state; do not acquire the active
  execution lease, call an LLM, or mutate the session to render inspection.
- Live control: `/stop <id>` uses existing exact-target ownership and cancellation
  rules. It must not be silently interpreted as stopping the current foreground run.
  Revalidate target identity and races; if another decision would be required, reject
  with guidance instead of nesting approvals. Live/modal `/stop` without an exact ID
  returns usage guidance; its existing idle selector can remain. Foreground
  cancellation remains Ctrl+C.
- Idle-only: session/model/config changes, `/questions` recovery,
  `/continue <plan-id>`, and other management commands unless explicitly classified
  otherwise. Explain why unavailable.

TUI F2 opens a command-only entry over a pending prompt; Escape returns with the
original prompt/draft intact. With no pending prompt F2 is a no-op; ordinary composer
slash commands remain the command path. Plain-chat pending prompts accept
`:command <slash-command>` for the five read-only inspections and `/stop <id>` only;
`text:` escapes literal question answers beginning with that prefix. One-shot exposes
only approval-local `details`, not a new slash-command surface. Unsupported command
entry is reported clearly, never treated as approval or an empty answer. Hints show
these rules.

In the ordinary composer, Enter sends when idle and queues when running; Ctrl+S
steers only the exact active run. Ctrl+C cancels an active run even with a selection
or modal open; when idle it clears a draft/selection and never doubles as copy or
implicit quit. Ctrl+Q explicitly quits through bounded cancellation/reconciliation.
Keep its two-press 1000ms confirmation window: the first press only arms the hint,
and a second Ctrl+Q within the window starts bounded shutdown. Timeout, any other
key/action, or a change of input consumer disarms it; Ctrl+C cannot confirm quit.
The first press neither cancels a run nor resolves a prompt. `/quit` and its existing
aliases remain explicit single-command quit requests when command entry is available.
Escape dismisses the top non-authorizing view; at a question it leaves unanswered,
at an approval it denies. Plain terminals retain OS interrupt/EOF behavior and
document supported text equivalents rather than claiming all TUI keys work there.
One-shot must install an interrupt owner and pass the existing cancellation signal
into its runtime: SIGINT during a question, approval, or tool requests cancellation,
awaits bounded reconciliation, and exits unsuccessfully (130 for Unix SIGINT), rather
than abruptly abandoning state. Native Windows qualification must deliver Ctrl+C
through the supported console/ConPTY path to the exact built one-shot binary while
a question, approval, and tool are active. Assert actual interruption, unsuccessful
exit, durable cancellation, bounded descendant cleanup, and terminal restoration;
injecting a raw byte without proving signal delivery is insufficient. Windows does
not inherit Unix exit code 130 as a requirement. Unsupported injection leaves the
gate open; N/A is not acceptance for this supported Windows capability.

Keep `/copy` as the portable explicit clipboard action and expose a labeled TUI copy
action without intercepting Ctrl+C. Show only applicable shortcuts in each state.
Preserve queue persist-before-ack, exact-run steering, cancellation retention, and
no crash-time automatic queue replay.

### 5. Output, presentation, and terminology

- `nib run --mode plan` prints ordered authoritative plan steps and the plan-only
  outcome. Completed one-shot execution prints the final validated assistant answer
  with paragraph/code structure, not a 512-character flattened synopsis. Existing
  safety/storage limits remain; any omission is explicit and identifies available
  persisted content, without promising recovery of content never retained. Do not
  expose private provider deltas or alter stable outcome tokens/exit statuses.
- Questions/approvals use a consistent order: heading, reason/context, choices or
  editable answer, local error, actions. Keep the active question/action distinguishable
  from background output; long content uses scrolling, not hidden approval controls.
- Distinguish user, assistant, and lifecycle/tool output with textual role labels in
  `NO_COLOR` and monochrome modes. Focus, selection, error, running, and blocked states
  cannot depend only on hue. Narrow layouts retain active actions and allow inspection
  of full sanitized content. Preserve T045 thought/tool scan-list styling otherwise.
- Plain session selection shows a readable name/goal, recency/status, and shortened ID;
  stable full identity is used internally and is available in details.
- `/review` remains a compatibility alias for diff inspection. Help and results say
  `Show changes (diff)` and never claim a code review was performed.
- `/copy` attempts clipboard delivery in supported interactive terminals. A successful
  native backend may report `Copied`; unacknowledged OSC52 reports `Copy requested`;
  unsupported/failing delivery offers a labeled selectable/text fallback. Redirected
  output never emits terminal clipboard escape sequences. Existing copy-on-release
  behavior is retained and follows the same truthful feedback and redaction rules.
- Say `Press Enter to continue`, `Waiting for your answer`, `Leave unanswered`, and
  `No active run to cancel` where accurate. Provider failures retain the existing typed,
  redaction-safe explanation and recovery action; do not relabel them validation errors.
- Update README, user guide, permissions documentation, inline help, and native smoke
  expectations together. Do not document behavior before the implementing increment.

## Affected Areas and Dependencies

- `src/interactive.rs`, `src/chat.rs`, `src/console.rs`, `src/tui/`: shared reducers,
  command availability, prompt ownership, editing, rendering, clipboard feedback.
- `src/agent/loop.rs`, `src/session/`: typed question outcomes, durable recovery,
  exact identity/provenance, dependency blocking and lease-safe reconciliation.
- `src/tools/executor.rs`, tool implementations/metadata as required: approval detail
  projection from validated arguments; preserve schema and policy enforcement.
- `src/run.rs` and CLI exit plumbing in `src/main.rs`: plan/final output,
  actionable waiting-for-input recovery, and reconciled interrupt exit behavior.
- MCP/gateway and delegated/durable callers: compatibility tests for missing handlers,
  fail-closed approvals, typed outcome adaptation, and unchanged external schemas.
- Tests, Taskfile wiring only as needed, native terminal smoke scripts, README,
  `docs/user/guide.md`, and relevant `docs/tech/` references.

[FT-019](../done/ft_019_codex_inspired_chat_and_tui_interactions.md) remains the umbrella.
This proposal explicitly revises selected T038/T039 defaults for Ctrl+C, role labels,
approval focus, and custom question answers. Coordinate with
[T045](../development/T045_codex_style_thought_and_tool_rows.md) and
[T046](../done/T046_cross_platform_ci_repairs.md); preserve scan-list presentation,
native terminal restoration, and platform-path fixes. T042 owns richer context
snapshots; T047 owns command availability and modal-safe access to whichever truthful
inspection data is currently available. Do not introduce a competing snapshot store.

Before editing shared smoke scripts, agree a file-ownership/handoff with T046 or
wait for that work to finish. Its completion is not a blanket prerequisite for
independent T047 work, and its previous green run cannot qualify changed behavior.
T043 retains broad module restructuring; T047 may extract small focused helpers to
meet the length gate. Neither task must globally wait for the other, but overlapping
module edits require explicit ownership/sequencing before implementation.

### Supersession and compatibility map

The following changes are intentional, not accidental drift. On the implementing
increment, append dated supersession notes linking to T047 in affected T038/T039/
FT-019 sections; retain their historical checked criteria and original validation.
Update affected T042/T045/T046 contracts/help/smokes in the same coordinated change.
Do not rewrite done-spec history or claim these new defaults already ship.

| Previous contract / owner | T047 decision | Delivery |
| --- | --- | --- |
| T039 Ctrl+C copies selected transcript | Ctrl+C cancels active work or clears idle input/selection, never copies | B2 |
| T038/T039 idle empty double Ctrl+C quits | Removed; use confirmed Ctrl+Q or explicit `/quit` | B2 |
| T038 double Ctrl+Q within 1000ms | Retained; no shared confirmation with Ctrl+C | B2 |
| T039 Approve-first Enter and immediate `y/n/1/2` | Default negative; buffered affirmative text or deliberate selection plus Enter; no digit approval shortcut | A3 |
| T039 immediate question digits / `Y` chooses first option | Editable answer and Enter submission; `Y` is text; Tab stays in modal | A2 |
| T039 `Skip` | `Leave unanswered`, reconcile to idle waiting state | A2 |
| T039 speech has no printed role labels | Textual roles in no-color/monochrome presentation | C |
| T045 drag-release copying | Retained with truthful feedback and redaction | C |
| T042 modal `/context` entry undecided | T047 supplies access; T042 supplies richer accounting behind it | B1 |
| T039/T046 startup/switch prompt mechanics | Shared safe defaults; unchanged workspace grant scope, terminal gating and switch authority | A3 |
| FT-019 shared command and runtime authority | Extended with `/questions` and exact-plan `/continue`; authority/persistence invariants retained | A2 |

## Alternatives Considered

- Patch each renderer independently: quicker initially, but repeats parsers and leaves
  lifecycle differences untested. Use shared semantic reducers with thin adapters.
- Make every surface identical: would burden one-shot automation and external adapters
  with chat behavior. Harmonize meaning while preserving intentional capabilities.
- Add optional questions and automatic resume now: expands dependency and execution
  authority. Keep questions required and resumption explicit in this task.
- Require retyping the original goal after recovery: cannot reliably reconstruct
  bounded/redacted text. Use an explicit exact-plan continuation action instead.
- Rewrite the TUI or reorganize all large modules: unrelated risk. Extract only the
  small shared helpers necessary for testable contracts and the function-size gate.

## Risks and Mitigations

- Accidental approval: default deny, explicit affirmative focus, identity binding,
  hostile/long argument fixtures, and stale-response rejection tests.
- Lost or misapplied answers: durable acknowledgement, exact recovery identity,
  conservative legacy migration, concurrent-writer and replay tests.
- Detail leaks/terminal injection: reuse redaction and display sanitization; bound
  inputs/pages and test controls, secrets, Unicode, and clipboard fallback output.
- Modal command/input ambiguity: explicit command entry, `text:` escape, retained
  input ownership, and tests for command-looking literal answers.
- Shortcut/script regressions: release notes, help changes, stable machine outcomes,
  native Linux/macOS/Windows smoke evidence, and no unqualified clipboard claims.
- Scope overlap: land three independently reviewed increments; do not take over T042
  accounting or treat T045/T046's earlier validation as evidence for new changes.

## Rollout Plan

Follow the companion plan in dependency order. Keep the spec in development until
all three increments and cross-surface/native gates pass. Each increment updates its
tests and affected help/docs in the same change. No hidden feature flag may allow
approval/parser semantics to diverge between terminal surfaces. New persisted fields
are additive/defaulted; old sessions remain readable and unanswered state stays safe.
Record behavior changes in release-facing documentation. Review spec compliance first,
then quality/security, and record exact-revision evidence before moving to `done/`.

## Acceptance Criteria

- [x] AC1: TUI/plain/console share numeric/text/option parsing and local retry semantics;
  no invalid answer consumes the request or triggers a model call.
- [x] AC2: TUI questions support custom answers, normal editing, Unicode and multiline
  paste; the question remains inspectable and initial Enter supplies no implicit answer.
- [x] AC3: Dismissal, EOF, cancellation, and absent handlers have distinct typed
  handling; all leave required dependencies blocked unless an answer is persisted.
- [x] AC4a: Leave unanswered reconciles the worker to idle waiting-for-input, making
  `/questions [id]` usable immediately and after restart, without automatic execution.
- [x] AC4b: `/continue <plan-id>` uses the persisted original goal and exact plan
  identity, including when its displayed summary is redacted/truncated; test both
  `agent.answer_only` settings without weakening policy or fresh-request semantics.
- [x] AC4c: Concurrent, stale, foreign, completed, malformed, or genuinely ineligible
  continuation targets fail closed based on blocking causes, not a coarse Blocked
  label. Test answered clarification alone and with a pending required check; retain
  verification obligations and unresolved authority/uncertain-effect blockers.
  Recovered answers cannot resolve another record; a deterministic recovery fixture
  executes the intended dependent action once.
- [x] AC4d: A premature tool-free completion with unresolved required verification is
  rejected without completing the step and receives a same-run corrective turn with
  the exact obligation IDs when bounds allow. Exact audited verification can then
  complete the same plan; repeated attempts or exhausted bounds remain terminal.
- [x] AC5: Approvals show common summary and inspectable redacted action details,
  bind to exact validated input, and accept only deliberate affirmative decisions.
- [x] AC6: Read-only live/modal commands and exact-target `/stop` work without stealing
  answers, granting consent, mutating inspection state, or weakening lease fencing.
- [x] AC7a: Ctrl+C never copies/quits; active cancellation takes precedence over
  selection/modal input, and idle clearing does not authorize or submit work.
- [x] AC7b: Ctrl+Q's first press only arms; its second within 1000ms quits through
  bounded reconciliation. Timeout, intervening input, and consumer changes disarm it.
- [x] AC7c: F2 and `:command` preserve prompt/draft ownership; idle F2 is a no-op.
  Prefix case, separators, empty/nested payloads, and modal frame behavior are tested.
- [x] AC7d: Enter send/queue, Ctrl+S exact-run steering, persisted queue retention,
  and cancellation/quit reconciliation retain their existing authority guarantees.
- [x] AC7e: One-shot Unix SIGINT exits 130 after bounded reconciliation; native
  Windows interruption proves non-success, cancellation, cleanup, and restoration
  against the exact binary. Missing native evidence is not N/A or a pass.
- [x] AC8: One-shot plan/final output is complete within declared safety limits,
  structured, redaction-safe, and compatible with existing outcome/exit-code contracts.
- [x] AC9a: No-color/narrow layouts expose roles, focus, states, actionable errors,
  and readable session choices without hidden-only actions.
- [x] AC9b: `/copy` and retained drag-release copy report native success, unconfirmed
  OSC52, and failure accurately; redirected output contains no clipboard sequences.
- [x] AC9c: `/review` explicitly means diff inspection, not performed code review.
- [x] AC10a: Docs/help and dated supersession notes match implemented behavior;
  T042/T045/T046 ownership is reconciled without adopting T043's broad restructuring.
- [x] AC10b: Workspace consent and management/switch confirmations use safe shared
  input defaults; deny/EOF grants nothing, changes no management/session state, and
  preserves terminal-only workspace gating and existing grant scope.
- [x] AC10c: Typed question adapters preserve legacy implementer compatibility;
  absent handlers, explicit deny/require-approval policies, external schemas, and
  legacy `approve_plan` compatibility remain fail-closed without adding a plan gate.
- [x] AC10d: All implementation gates and exact-revision native evidence are recorded;
  every subcriterion has its own linked fixture/evidence, not only an aggregate claim.

## Validation Gates

Spec authoring: `task docs:check`, whitespace/link review, and source-grounded spec
compliance plus quality/security review. These validate the proposal, not its future
implementation. Implementation requires:

- Per increment: `task check`, `task test:interactive`, `task docs:check`, and the
  relevant session/runtime tests through a focused Task target.
- Runtime/lifecycle changes: `task test:runtime-e2e` and `task test:tui-shutdown`.
- Final local gate: `task verify`; configured CI coverage remains mandatory.
- Native qualification: `task smoke:interactive` on Linux/macOS and
  `task smoke:interactive:windows:binary` against the exact built Windows binary;
  retain before/after terminal restoration and sanitized capture evidence. Tests use
  Mock/local fixtures, bounded timing, and no paid/provider credentials.

The companion defines the scenario matrix. Unrun or failing gates remain explicit
blockers; a unit snapshot alone is not proof of usable native input or clipboard support.

## Review Question Decisions

The user accepted the review recommendations on 2026-09-19. This record resolves
the 20 questions added by independent review; the corresponding design and acceptance
criteria above are normative. Resolution approves this backlog design, not implementation
completion or a lifecycle transition.

| Review ID | Decision | Implementation / acceptance |
| --- | --- | --- |
| Q1 | Record explicit supersession; annotate older specs when the implementing change ships, preserving history | Supersession map; AC10a |
| Q2 | Ctrl+C cancels active work or clears idle input/selection, never copies | B2; AC7a |
| Q3 | Remove idle double-Ctrl+C quit; retain explicit quit paths | B2; AC7a–b |
| Q4 | Retain two Ctrl+Q presses within 1000ms; no cancellation on first press or Ctrl+C confirmation | B2; AC7b |
| Q5 | T047 owns modal/live context access; T042 replaces its data behind the same entry point | B1; AC6, AC10a |
| Q6 | Invalid approval text retries; EOF denies with typed InputClosed, not cancellation | A3; AC5 |
| Q7 | Shared negative defaults apply to consent/management/switch confirmation; preserve authority and denial consequences | A3; AC10b |
| Q8 | Leave unanswered reconciles to idle waiting-for-input; recovery is available without restart | A2; AC4a |
| Q9 | Use explicit exact-plan Continue; never require reconstruction of displayed goal text | A2; AC4b–c |
| Q10 | Explicit continuation uses existing execution, not fresh answer-only classification; existing one-shot routing remains compatible and tested | A2; AC4b |
| Q11 | Digits insert answer text until Enter; no Y-first-option shortcut; Tab cycles inside the modal | A2; AC2 |
| Q12 | Case-sensitive, space-separated prefixes; empty/malformed payloads error locally; text escape is single-pass and commands remain in-frame | A1/B1; AC1, AC7c |
| Q13 | F2 only opens prompt-local command entry; without a pending prompt it is a no-op | B1; AC7c |
| Q14 | Add a typed contextual method with a conservative legacy default; runtime never parses error text | A1; AC3, AC10c |
| Q15 | Keep drag-release copying, with truthful redaction-safe feedback | C; AC9b |
| Q16 | Reachable legacy approve_plan prompts share rendering; normal flows gain no plan approval gate | A3; AC10c |
| Q17 | Native Windows exact-binary interruption evidence is mandatory; unsupported injection is an open gate, not N/A | B2/C; AC7e |
| Q18 | Split broad acceptance groups into individually evidenced subcriteria | AC4a–c, AC7a–e, AC9a–c, AC10a–d |
| Q19 | Settle T046 shared-file ownership before editing those files; independent work need not wait for its whole spec | Preparation; AC10a |
| Q20 | Small helper extraction is allowed; broad splitting stays with T043, with overlapping edits explicitly sequenced | Preparation; AC10a |

## Open Questions and Qualification

No product decision from Q1–Q20 remains open. Shared-file handoffs and native F2,
clipboard, and Windows interruption evidence remain implementation prerequisites/gates,
not assumptions that the behavior already works. The historical full-verification
failure in the companion remains recorded until a new successful run supersedes it.

## Implementation Checkpoint (2026-09-21)

The recovered-answer path now holds the authoritative run lease through identity
revalidation and durable publication. TUI and plain recovery retain the responder on
invalid input, and runtime coverage proves that an answered clarification continues
the exact stored plan once with `agent.answer_only` both enabled and disabled. The
continuation adds an explicitly runtime-authored user-role boundary while preserving
the saved human goal and plan identity.

Approval details are generated from the validated invocation, redacted and
control-sanitized before lossless paging, and exposed through a non-authorizing TUI
detail view. Ctrl+Q is disarmed by timeout, intervening input, and consumer changes.
One-shot output suppresses the duplicate streamed final response and emits the
complete reconciled retained answer once within the declared 64 KiB public-output
limit, with an explicit omission marker for incompatible oversized legacy state.
`/copy` now attempts delivery, distinguishes native success from unconfirmed OSC52
and failure, and emits no clipboard escape sequence when redirected. Plain resumed
history now uses the shared public conversation projection and cannot expose provider
tool envelopes or raw tool-result payloads.

Focused evidence passed in this working tree: `task check`, `task test:interactive`,
`task test:agent-context`, `task test:runtime-e2e`, `task test:tui-shutdown`,
`task docs:check`, `git diff --check`, the complete local `task verify`, and
`task coverage` at 85.78% line coverage (113,996/132,896). The final successful
verification included 1,174 library tests, 89 binary tests, and every
ordinary integration target; only explicitly opt-in paid/live and release-binary
qualification tests remained ignored by their existing contracts. The final Linux
optimized-binary sequence (`task build` then `task smoke:interactive:binary`) also
passed. It proves native F2 command routing without consuming the pending question,
OSC52/native-copy status handling, native Ctrl+C while a one-shot question, approval,
and foreground tool are active, exit 130 with durable cancellation, privacy scanning,
and exact before/after terminal-mode restoration. Sanitized captures are retained
under `target/t047-local-evidence/Linux/` for this working tree. The smoke summary
records the source revision and embedded binary identity, but correctly marks this
dirty working-tree run ineligible as final acceptance evidence.

This spec remains in development. macOS qualification was not run on this Linux host.
Windows ConPTY qualification was prepared in `scripts/check-interactive-release.ps1`,
but the local Windows task could not run because `pwsh` is unavailable. Exact-revision
native F2, clipboard, and interruption evidence still need the hosted Windows/macOS
matrix. Therefore AC7c,
AC7e, AC9b, and AC10d remain open, and no exact-revision completion claim is made from
this uncommitted working tree.

## Exact-Revision Qualification Evidence (2026-09-23)

The implementation revision `62d45bd468bc78af2f7f38efe170cdfdcd938498`
passed local `task verify`, including 1,174 library tests, 89 binary tests, and
the ordinary integration suites. [CI run 35800536448](https://github.com/skills-yaml/nib/actions/runs/35800536448)
built the clean merge revision `5a31c4c739fc3f0190e6c74395e6e282cd9a5db4`.
Linux validation passed with 85.80% runtime line coverage (114,022/132,898).
The [Linux](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726522697),
[macOS](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726782294),
and [Windows](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702)
native artifacts each record that exact revision, a clean source tree, eligible
binary identity, F2 and plain-modal command success, three durable interruption
cancellations, terminal restoration, and a passed privacy scan. macOS and Windows
reported native clipboard success; Linux accurately reported an unconfirmed OSC52
request. All three CI jobs, including the ordinary Windows full test suite, passed.
The final spec-compliance review found each AC covered by its own fixture or native
capture. The subsequent quality/security review found no unresolved blocker:
session reads permit Windows writer/delete sharing, native waits are bounded and
exact-session scoped, approval and cancellation retain durable reconciliation,
and the captured output passed the privacy scans. The dated checkpoint above is
historical; its open-gate statement was superseded by this qualification.

Each acceptance subcriterion has distinct observable evidence rather than inheriting
an aggregate suite result. Source links identify the named fixture in that file;
the three artifact links above hold sanitized native captures and summaries.

| Criterion | Linked fixture or qualification evidence |
| --- | --- |
| AC1 | [Shared prefix/retry grammar](../../../src/interactive.rs#L5795), [plain modal retry](../../../src/chat.rs#L3105), [TUI typed response](../../../src/tui/mod.rs#L8880) |
| AC2 | [TUI typed response](../../../src/tui/mod.rs#L8880), [Unicode/multiline paste](../../../src/tui/mod.rs#L8904), [question card](../../../src/tui/mod.rs#L10038) |
| AC3 | [Shared modal outcome reducer](../../../src/interactive.rs#L5895), [closed plain input](../../../src/chat.rs#L3054), [legacy handler fallback](../../../src/tools/executor.rs#L3442) |
| AC4a | [Durable exact recovered answer and run lease](../../../src/interactive.rs#L5372), [TUI persistence before close](../../../src/tui/mod.rs#L8957) |
| AC4b | [Exact-plan recovery with answer-only on/off](../../../src/agent/loop.rs#L10816), [redacted display remains non-authoritative](../../../src/interactive.rs#L5372) |
| AC4c | [Continuation eligibility and required checks](../../../src/agent/loop.rs#L8816), [malformed target fails closed](../../../src/agent/loop.rs#L10920) |
| AC4d | [Premature-completion corrective runtime fixture](../../../tests/test_runtime_e2e.rs#L895), [required-check admission](../../../src/agent/loop.rs#L8816) |
| AC5 | [Bounded redacted approval context](../../../src/tools/executor.rs#L3361), [narrow approval card](../../../src/tui/mod.rs#L10081) |
| AC6 | [Live queue/steer reducer](../../../src/interactive.rs#L6570), [plain exact-run router](../../../src/chat.rs#L2381), [native modal capture](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702) |
| AC7a | [TUI question cancellation](../../../src/tui/mod.rs#L9075), [approval-blocked shutdown](../../../src/tui/mod.rs#L9122), [native question/approval/tool cancellation captures](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702) |
| AC7b | [Uninterrupted two-press Ctrl+Q fixture](../../../src/tui/mod.rs#L7079) |
| AC7c | [Case/separator/empty/nested grammar](../../../src/interactive.rs#L5795), [idle F2/draft ownership](../../../src/tui/mod.rs#L8927), [native F2 and plain captures](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702) |
| AC7d | [Durable FIFO queue/start failure](../../../src/interactive.rs#L6930), [exact-run steering admission](../../../src/agent/loop.rs#L8228), [plain cancellation retention](../../../src/chat.rs#L2588) |
| AC7e | [Unix exit-130 SIGINT fixture](../../../tests/interactive_cli.rs#L518), [Windows native three-stage captures and restoration](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702) |
| AC8 | [Ordered one-shot plan/no execution](../../../tests/interactive_cli.rs#L296), [long structured final output](../../../tests/interactive_cli.rs#L267) |
| AC9a | [Narrow/Unicode transcript fixture](../../../src/interactive.rs#L6109), [no-color approval signaling](../../../src/tui/mod.rs#L6673), [narrow question/approval cards](../../../src/tui/mod.rs#L10038) |
| AC9b | [Backend success/OSC52/failure outcomes](../../../src/tui/mod.rs#L10408), [drag-release route](../../../src/tui/mod.rs#L10329), [redirected pipe omits escapes](../../../tests/interactive_cli.rs#L335), [Windows native clipboard](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702), [Linux OSC52 labeling](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726522697) |
| AC9c | [Owned-worktree `/review` fixture](../../../src/interactive.rs#L6824), [user-facing review wording](../../../README.md) |
| AC10a | [Documentation integrity fixtures](../../../tests/docs_integrity.rs), [supersession ownership](#review-question-decisions), [release smoke contract](../../../tests/installers.rs#L324) |
| AC10b | [TUI workspace consent](../../../src/tui/mod.rs#L7001), [plain session switch/deny](../../../src/chat.rs#L2771) |
| AC10c | [Contextual legacy handler default](../../../src/tools/executor.rs#L3442), [legacy `approve_plan` context](../../../src/tools/executor.rs#L3625), [closed-input runtime fixture](../../../src/chat.rs#L3054), [explicit policy denial](../../../tests/test_runtime_e2e.rs#L2095), [MCP deny-without-handler/schema fixtures](../../../src/integrations/mcp_server.rs#L3647) |
| AC10d | [Exact-revision CI run](https://github.com/skills-yaml/nib/actions/runs/35800536448), [Windows](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726815702), [macOS](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726782294), [Linux](https://github.com/skills-yaml/nib/actions/runs/35800536448/artifacts/10726522697), and this per-criterion map |
