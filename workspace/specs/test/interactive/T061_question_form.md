# T061: Question Form

**Status:** Test — implementation integrated into shared development.

State: test
Primary Feature: interactive

Contract accepted on 2026-09-30, revised by the user on 2026-10-01;
implementation started by user request on 2026-10-05.

**Related:**
[T047: User Interaction Harmonization](../../done/interactive/T047_user_interaction_harmonization.md),
[T050: Plan-Free Answers, Decision Prompts, and Run Outcomes](../../done/interactive/T050_plan_free_answers_decision_prompts_and_run_outcomes.md),
[T049: Command Approval Card](../../done/interactive/T049_interaction_card.md)

## Problem

`ask_question` accepts one question and a list of plain strings. The TUI puts
that text in the composer and draws a one-line choice band. With options, the
draft the user types is not shown. Focus starts in a hidden editor, so the
selection chevron is absent until Tab. The footer still says `Esc skip`. A
model that needs several related answers has to stop after each one, wait for
the run to resume, and ask again. Option text cannot carry a short label plus
an explanation. There is no way to leave the form and keep talking without the
Esc outcome, which marks the clarification unresolved and pauses the run.

## Scope

The `ask_question` contract, the shared question parser, the TUI form, the
plain, console, and one-shot line protocol, clarification persistence for a
set, conversational recovery, removal of `/plan` and `/questions` from the
interactive command surface, the execution prompt, and the user guide.

## Goals

- One `ask_question` call carries either one question or a set of at most eight.
- One question uses the card below and submits that answer directly.
- Two or more questions use the same card under a tab strip, keep each draft,
  and send every answer on Submit.
- Each choice is a label plus an optional description. The stored answer is the
  label.
- `Type something.` is a visible row, and the text the user types is visible.
- `Chat about this` returns that message to the model in the same run and does
  not mark any question in the call answered.
- Esc interrupts the current operation and leaves the call unanswered.
- A later answer resumes work automatically. A request to resume reopens any
  unanswered questions before dependent work continues.
- Specs govern development planning and spec writing; `/plan` and `/questions`
  are not needed in the user interface. Internal runtime plan state remains
  authoritative for execution and reconciliation.
- The TUI, plain chat, the console, and one-shot `nib run` offer the same
  choices. Plain, console, and one-shot stay line-oriented.

## Non-Goals

- Optional questions, multi-select, and skipping one question inside a set.
- Treating a chat message or an unanswered question as tool permission or as
  consent for dependent work.
- Changing command approval, workspace consent, or session-switch cards.
- Replacing the T050 proposed-answer labels.
- A new question tool, a second reducer, or relaxing the rule that
  `ask_question` is the only tool call in its batch.
- Rewriting historical sessions or repairing already stored clarifications.

## Users and consumers

People answering nib in the TUI, plain chat, the console, and one-shot runs.
The model consumes the tool result. Session recovery consumes the same
clarification records through ordinary conversation and automatic recovery.

## Decisions

- A call uses either the legacy `question` field or the `questions` array, and
  never both. An array of one item is the single-question card. An array of two
  or more is the tabbed form. The legacy field remains the single-question card.
- A string option remains valid and is a label with no description. An object
  option is `{ "label", "description?" }`. Duplicate labels in one question are
  rejected. Descriptions are not part of the answer.
- The selection marker is `› ` (U+203A plus one space), the same marker as the
  command card. Other rows start with two spaces. The first choice starts
  selected when the question has options. Enter confirms the visible row.
- This supersedes the T047 rule that the TUI question starts in an empty editor
  so bare Enter cannot choose the first option. Enter now confirms the row under
  the chevron. Digit keys on the choice list move the chevron and do not submit.
  In the text editor, digits are text. `Y` is text, not an alias for option 1.
- `Type something.` is always the last numbered row of the question. Enter on
  that row opens the editor. Esc in the editor interrupts the operation and
  leaves the question unanswered. A non-empty editor submission is the custom answer.
  Empty text is a local retry.
- `Chat about this` is numbered, drawn below a rule, and applies to the whole
  call. Enter asks for a message. An empty message retries. The message returns
  to the model as a successful `discussed` result. Drafts from the call are
  discarded. No question in the call becomes answered. The run stays in
  progress. Dependent paths stay blocked until a later call is actually
  answered. Discussion is not an answer or permission to execute dependent work.
  The pending questions remain durably linked to their operation for later
  conversational recovery, including after restart.
- Esc anywhere in the question form, a plain/console/one-shot line that is exactly
  `esc`, EOF, and cancellation interrupt the current operation. Esc's visible
  name is `Interrupt operation`. It is not a numbered row. Reconcile and stop
  the active worker with the existing `waiting_for_user_input` outcome; do not
  leave an execution worker running. Unsubmitted drafts are discarded.
  Questions that were not already answered remain unresolved. This also applies
  to the custom-answer editor and the discussion-message editor.
- In interactive conversation, an answer to the interrupted operation's pending
  question updates the clarification and resumes that exact operation
  automatically once its required answers are complete. A request to resume
  without those answers reopens the pending form before dependent work runs.
  No `/questions` or `/continue` command is required. Unrelated messages do not
  answer or resume an interrupted operation. If more than one operation could
  match, ask which operation the user means before resuming. Recovery after
  restart uses persisted operation and clarification identity. A completed,
  cancelled by an explicit stop request, or superseded operation is not silently
  restarted by a later answer. One-shot runs that have exited preserve their
  pending records for recovery in the same session through interactive chat.
- On a proposed-answer question the three T050 rows stay exact: `Approve
  proposed answer`, `Reject and leave unanswered`, and `Instruct otherwise`.
  `Instruct otherwise` is that question's text editor. Model options stay
  hidden when a proposal is present. Reject and Esc leave the whole call
  unanswered and interrupt the operation. Approve stores exactly the displayed
  proposal. Add a fourth numbered row, `Chat about this`, below a rule. It has
  the same whole-call discussion behavior as an ordinary question, including
  in a set containing both ordinary and proposed-answer questions.
- Every question in a set needs a draft before Submit. Submit on an incomplete
  set is a local retry and focuses the first unanswered question. Choosing an
  option or submitting text stores that draft, checks the tab, and moves to the
  next unanswered question, or to Submit when the set is complete. Left and
  Right move between tabs, including Submit, and wrap. A later change replaces
  that question's draft.
- Plain, console, and one-shot ask each question in order. On a set, after the
  last answer they print the drafts and wait for one submit line. Enter submits.
  A question number reopens that question. There is no submit line after a
  single question.
- Exact reuse on the same plan stays. The match is question text, proposal,
  normalized options (label and description), and plan. A changed description
  asks again. A single-question match returns the stored answer without a
  prompt. In a set, a match is pre-checked and still shown; if every question
  matches, the call returns the stored answers without a prompt.
- `text:` remains the literal-answer escape on every surface. `text: chat` and
  `text: 1` are answers, not the chat row and not option 1. A line that is
  exactly `chat` selects `Chat about this`.
- An answer never approves a tool and never waives verification. Invalid input
  stays a local retry and does not consume the responder.

## Card

### One question

```text
Are the design prices final, and how is VAT shown?

› 1. Final, + IVA shown
     Keep 19/57 and 49/147; the UI shows '+ IVA'.
  2. Final, IVA inclusa
     Same prices, already including 22% VAT.
  3. Not final yet
     Read prices from config until the numbers are final.
  4. Type something.
────────────────────────────────
  5. Chat about this
```

An optional `header` is one line above the question. With no options, row 1 is
`Type something.` and it is selected. A description wraps under its label and
indents with the label text. The question stays visible while the list scrolls.

### Several questions

```text
←  ☐ Prices/VAT  ☐ Interval  ☐ Trial charge  ☐ Invoices  ☐ Submit  →

Are the design prices final, and how is VAT shown?

› 1. Final, + IVA shown
     Keep 19/57 and 49/147; the UI shows '+ IVA'.
  2. Final, IVA inclusa
  3. Not final yet
  4. Type something.
────────────────────────────────
  5. Chat about this
```

A question tab shows `☐` until it has a draft and `✔` after it has one. Submit
shows `✔` only when every question has a draft, and `☐` otherwise. The focused
tab is visually distinct. `←` and `→` appear when the strip overflows. The TUI
footer for the choice list is `Up/Down select · Enter choose · Esc interrupt
operation`. The editor footer is `Enter submit · Esc interrupt operation`. The stale
`Enter / 1-9 answer · Esc skip` footer is removed.

### Plain, console, and one-shot

```text
Question 1/4 — Prices/VAT
Are the design prices final, and how is VAT shown?

  1. Final, + IVA shown
     Keep 19/57 and 49/147; the UI shows '+ IVA'.
  2. Final, IVA inclusa
  3. Not final yet
  4. Type something.
────────────────────────────────
  5. Chat about this

Answer (number, text, or chat):
```

A single question omits the `Question k/n — title` line. The prompt is the same.
After a complete set:

```text
Prices/VAT: Final, + IVA shown
Interval: Monthly

Submit these answers? Enter submits, a number reopens that question:
```

## Contract

`ask_question` gains optional `header` and `questions` without removing
`question`, `proposed_answer`, `options`, or `dependent_paths`.

- `header`: at most 500 bytes.
- `questions`: 1 to 8 objects. Each has `question` (at most 20,000 bytes),
  optional `proposed_answer` (at most 20,000 bytes), optional `options` (at most
  20), and `title`. `title` is required when the array length is greater than
  one, unique in the call, and at most 40 bytes. It is the tab label.
- Option label: at most 200 bytes. Description: at most 1,000 bytes. A legacy
  string option: at most 1,000 bytes.
- `dependent_paths` stays call-level. Empty still means the whole current plan
  step.
- Supplying both `question` and `questions`, an empty question, a missing title
  on a set, a duplicate title or label, or an over-long field rejects the call
  before the handler is invoked.
- Titles, descriptions, headers, and chat messages pass through the existing
  public-output bound and redaction before display or persistence.

Successful answers return one observation. A single question keeps today's
`question`, `options`, and `answer` fields and adds `source` of `option`,
`text`, or `approved_proposal`. A set returns `status: answered` and `answers`
in call order, each with `title`, `question`, `answer`, and `source`. A
discussed call returns success with `status: discussed`, the `message`, and the
question texts. It is not a tool error. Left unanswered, cancellation, and
input closure stay unsuccessful tool results with the existing outcome names.

Persistence adds backward-compatible fields only. Old records still load. A set
stores one clarification per question under the same invocation, with a stable
question index. Persist the exact session, operation/run, plan, invocation, and
question index needed to recover an interrupted or discussed call. Already
answered siblings stay answered; unsubmitted drafts do not become answers.

Discussion and interruption have separate lifecycle effects but both retain
unanswered dependency obligations. Dependency gating, completion checks, and
recovery must consult those obligations even when the tool observation is a
successful `discussed` result. A later answer resolves only its linked question;
a new question with similar text does not silently clear an old blocker. If a
question is revised during discussion, preserve an explicit link to the original
obligation. Reuse of an exact prior answer retains the existing matching rule.
Resolve the linked obligations before resuming dependent work, and preserve
that state across reloads. Ordinary conversation opens the pending form or
accepts an unambiguous answer; incomplete or ambiguous answers keep required
work blocked. Recovery uses the same set Submit and proposed-answer rules.

Remove `/plan` and `/questions` from command registration, completion, help, and
user documentation as part of implementation. Specs govern development planning;
this does not remove the internal runtime plan, step state, audit trail, or
multi-step progress display. Automatic resumption does not approve tools, bypass
verification, replay uncertain provider continuations, or waive existing
execution-admission and reconciliation checks.

The execution prompt tells the model it may send one question or a related set
in a single `ask_question` call, that the call remains the only tool in the
batch, that options should use a label and description when the choice needs an
explanation, and that a discussed message is conversation rather than the
answer or permission to continue dependent work.

## Affected Areas

- `src/tools/registry.rs`, `src/tools/core.rs`: schema and argument validation.
- `src/agent/loop/`: one-call parsing, reuse, outcomes, tool observation, and
  dependency gating.
- `src/agent/instructions.rs`: question-form prompt.
- `src/session/`: clarification fields and status for a discussed call.
- `src/interactive/`: shared parsing for numbers, `text:`, `chat`, `esc`, and
  batch submit.
- `src/tui/`: form layout, tabs, visible editor, footer.
- `src/chat/`, `src/console.rs`, `src/run.rs`: line protocol and one-shot wait
  text.
- `workspace/docs/user/guide.md`: replace the one-question prompt description.
- Interaction and runtime regression tests.

No persistence migration. Missing new fields mean a legacy single question.

## Compatibility

Legacy clients that send `question` and string `options` keep one question, one
answer, with conversational recovery replacing the slash-command workflow.
The visible card gains `Type something.` and `Chat about this`, and Enter
confirms the highlighted choice. Proposed-answer calls keep the three T050
labels and add `Chat about this`. MCP callers gain the new optional fields and
object options; unknown fields stay rejected.

## Acceptance Criteria

- [ ] A call with `question` and string options shows the one-question card, with
  the chevron on row 1, the description under an object option, and the typed
  draft visible. Enter on row 1 submits that label. Digits move the chevron.
  `Type something.` submits non-empty text. Empty input retries.
- [ ] A call with one `questions` item uses that same card and no tab strip. A
  call with two to eight questions shows tabs, keeps drafts while moving, and
  returns every answer only from Submit. Submit before each question has a draft
  retries on the first unanswered question.
- [ ] `Chat about this` on either form returns `status: discussed` and the
  message, leaves every question in the call unanswered, does not pause the run,
  and does not unblock `dependent_paths`. Esc, `esc`, EOF, and cancellation
  interrupt the operation from the list or either editor, stop its worker,
  and reconcile to waiting for input.
- [ ] A proposed-answer question still approves only the displayed proposal,
  rejects through `Reject and leave unanswered`, and takes a replacement only
  from `Instruct otherwise`. Model options stay hidden. A fourth row,
  `Chat about this`, works on single questions and mixed sets.
- [ ] Plain chat, the console, and one-shot print the same rows and descriptions.
  A set waits for the submit line. `text:` forces a literal answer. `chat`
  starts the message prompt.
- [ ] An exact reused single question does not prompt. A set pre-checks reused
  questions. A changed description asks again. A conversational answer resumes
  the exact interrupted operation automatically once required answers are
  complete. A resume request reopens the pending form. Discussed and interrupted
  obligations survive restart and block dependent work until their linked
  answers resolve them. Ambiguous or unrelated messages cannot resume an
  operation or clear another question's blocker.
- [ ] `/plan` and `/questions` are absent from registration, help, and completion.
  Internal plans, persisted step state, and progress remain consistent; recovery
  requires no slash command.
- [ ] A question answer, a discussed message, and a custom answer do not approve
  a tool or waive verification. Invalid calls and invalid input produce no extra
  model turn.
- [ ] Focused interaction and runtime tests, `task docs:check`, and `task verify`
  pass. The user guide matches the card.

## Implementation Plan

1. Add failing schema, reducer, persistence, and TUI/plain/console fixtures for
   one question, a set, discussed, Esc, proposal, reuse, restart recovery,
   conversational answers, ambiguous resume requests, and dependency blocking.
2. Extend the tool contract, loop outcome, and shared parser. Keep legacy
   `question` calls working.
3. Render the TUI form and the line protocol from that parser. Update the
   execution prompt and the user guide. Remove `/plan` and `/questions` from
   the interactive command surface and implement conversational resumption.
4. Review spec compliance, then run the validation gates. Record evidence and
   move this spec to `done` only after those gates pass.

## Validation Gates

- `task test:interactive`
- `task docs:check`
- `task verify`

The interactive gate covers the TUI card, plain and console lines, one-shot
waiting text, recovery, and the proposal rows. `task verify` covers the agent
loop, schema, session compatibility, and documentation links.

## Risks and Rollback

Enter on a single question now submits the highlighted label. The chevron shows
which label that is, and Esc interrupts the operation without answering.
A discussed result must stay successful for the model and unsuccessful for dependency
gating; folding it into either "answered" or "left unanswered" would either
authorize dependent work or stop the run. Rollback removes `header` and
`questions` from the schema and restores the single-question band. Old session
files remain readable because the new fields are optional.

## Prior Memory Notes

Record the user-approved 2026-10-01 direction as an accepted contract, not
shipped behavior: conversational resumption, interruption versus discussion,
proposal chat, and spec-based development planning without `/plan` or
`/questions`. Reconcile implementation evidence after verification. Do not
record session identifiers or sample answers.

## Contract Revision Evidence (2026-10-01)

The user approved Esc interruption, automatic conversational resumption after
an answer or resume request, active discussion with blocked dependencies,
proposal chat, and spec-based development planning without `/plan` or
`/questions`. Spec review reconciled decisions, persistence, recovery,
acceptance criteria, and implementation scope. Implementation remains pending;
no acceptance checkbox is promoted by this documentation update.

- `task docs:check` passed all five documentation checks.
- `task verify` passed outside the restricted sandbox: 1,252 library tests,
  93 CLI tests, all integration suites (including 54 runtime tests), and doctests.
  The initial restricted run failed fixtures requiring localhost and Unix
  socket creation with `Operation not permitted`; the unrestricted rerun passed.
- `git diff --check` passed.
- Reviewed the revised contract for spec compliance and consistency with the
  current runtime boundaries. Current user documentation remains unchanged
  until implementation lands, so it continues to describe shipped behavior.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-next | Preserves the accepted question-form contract documentation published in 0.2.0; all new runtime implementation is versioned by the linked T067 spec at 0.3.0. |

## Memory Impact

Status: pending

Rationale: The accepted user decisions remain recorded in memory. Resolve shipped form,
linked clarification recovery, compatibility, and delivery facts after exact
implementation/review evidence is available; update category and changelog then.

## Published Contract Version Boundary (2026-10-05)

The development 0.2.0 publication at
`8bc243d00ec3ec82e7287c704fcdd897ff78204a` includes this accepted-contract
documentation only. At that publication boundary implementation had not
started, and every implementation acceptance criterion remained unchecked at that publication.
The released nib-next membership is publication history and cannot authorize
future question-form code. Before implementation begins, obtain a fresh atomic
reservation from the latest published baseline and reconcile membership or
create a linked implementation spec. No new target is chosen by T062 delivery.

## Runtime Implementation Start (2026-10-05)

The user's request to continue T061 authorizes the approved implementation scope.
The atomic reservation is `nib-question-form`, minor 0.3.0 from 0.2.0. The
public development manifest was checked at commit
`3cc4550b7d807f39a5fecf07196ee2b703503b24`; it reports 0.2.0. The production
rolling release remains 0.1.0. No versioned 0.3.0 tag is occupied. Prior released
`nib-next` evidence preserves T061's documentation-only history; its active member
membership and minor impact remain preserved publication history; canonical member
paths follow lifecycle transitions. The linked
[T067 runtime implementation](T067_question_form_runtime_implementation.md) owns the
new 0.3.0 runtime membership without rewriting the released target, aggregate
impact, or claiming question-form implementation in 0.2.0.

Stable acceptance identifiers AC-1 through AC-9 refer in order to the nine
acceptance criteria above. The adjacent plan orders foundation, surface adapters,
conversational recovery, independent review, frozen gates, development integration,
main delivery and final lifecycle reconciliation. The confirmed shared integration event below establishes the test state.

## Implementation Evidence (2026-10-06)

The independently accepted combined implementation is integrated into shared
development. Main delivery and final lifecycle reconciliation are pending.

Confirmed shared development integration `a3b99632ae8cb452d5bfaff5555a3728999e8ba5` was pushed and its remote ref verified on 2026-10-06 at 06:37:20 UTC. Main delivery is pending.

| Criteria | Observable evidence |
| --- | --- |
| AC-1, AC-2 | Shared form reducer and TUI card tests exercise selection, visible editors, single-item arrays, checked drafts, wrapping tabs and explicit complete Submit. |
| AC-3, AC-4 | Agent/session and TUI tests exercise successful discussion with unresolved obligations, exact proposal sources, interruption worker reconciliation and stale/busy recovery closure. |
| AC-5 | Shared line adapter and CLI fixtures exercise sequential drafts, review/reopen/Submit, literal text, discussion and the plain framing boundary before persistence or continuation. |
| AC-6 | Guarded recovery tests exercise exact run/plan/invocation identity, restart, partial reuse, revised discussed obligations, ambiguous origins and concurrent plan replacement. |
| AC-7 | Interactive registration/help/completion and CLI fixtures exclude the removed commands; status retains saved one-step plan detail and verification diagnostics. |
| AC-8 | Schema, private outcome transport, leased persistence and agent admission fixtures reject malformed calls and preserve tool approval, verification and dependency gates. |
| AC-9 | Frozen exact-source task verify passed (1,335 library tests, 106 CLI tests, every integration suite including 54 runtime cases, and doctests); focused interaction (302), agent-context (201), and all five documentation checks passed. Native optimized-binary, PTY and managed-process qualification passed. CI 37419726793 passed Linux/macOS/Windows on the same clean exact source revision, with 84.48% Linux line coverage (104,572/123,780; required 80%). |

Independent source spec-compliance review preceded quality/security/integrity/interface
review on checkpoint `e8e2c8dd58ced7e1ccb386fe837b6f3fee7ed0d7`; neither found a
confirmed defect. Subsequent HTTP fixture, ConPTY prompt and plain-worker portability repairs
received renewed independent exact review and passing native integration gates.
Whole-spec AC-1 through AC-9 acceptance was independently approved for internal
integration `f8b09bf5c26ab2ce6f7ef0b5f119abf83a38b782`, whose tree is identical to
the qualified shared candidate. No paid LLM qualification was run.

## Exact Qualification and Delivery Evidence

Qualified source: `a3b99632ae8cb452d5bfaff5555a3728999e8ba5`;
clean tree: `6fee5b368b59753947cd7ea54fe2de1490000bdd`.
Native frozen gates passed in order: `task verify`, `task test:interactive`,
`task test:agent-context`, `task docs:check`, `task qualify:llm-release`,
`task smoke:interactive:binary`, and `task smoke:managed-process`.
The optimized executable SHA-256 stayed
`28afddd240901ba0e9fa60a5493b29cedb0c4d427e5451e5374dec4a15673b3f`
through the native smoke gates. All hosted qualifiers report this exact source
revision and a clean worktree in
[CI 37419726793](https://github.com/skills-yaml/nib/actions/runs/37419726793).

| Platform | Qualified executable SHA-256 |
| --- | --- |
| Linux | `a60d41e563170f765afedb61b8d4df7d73b6b19ae9b3ab2d73cb067868074eb2` |
| macOS | `28d012b4eda2d3a75cc93d1d71ecb206a5342e233e71cc918200c01834d3290e` |
| Windows | `2f190b86acf713d6143382c6dd39b1c3c8bffa1e16a018fcd4252ed3ee7f101c` |

The Windows gate includes the unchanged live-input Esc reconciliation regression.
A native 1 MiB main-thread regression and worker-unwind test verify that routing
polls the agent on its existing 4 MiB worker and cancels it when routing unwinds.
Development/main branch publication remains a separate event from public archives.

## Integration Evidence

Revision: a3b99632ae8cb452d5bfaff5555a3728999e8ba5
Outcome: passed

The exact qualified source was pushed to shared development and its remote ref
verified on 2026-10-06 at 06:37:20 UTC. Independent whole-spec acceptance, frozen
native Task gates and Linux/macOS/Windows CI 37419726793 passed before this event.
