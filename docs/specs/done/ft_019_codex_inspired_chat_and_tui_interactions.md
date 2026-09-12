# FT-019: Codex-Inspired Chat and TUI Interactions

**Status:** Done

**Related:**
[T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T025: Interactive Chat and TUI Capability Parity](../done/T025_interactive_chat_tui_capability_parity.md),
[T028: Current-Session-First TUI and Slash-Command Completion](../done/T028_current_session_first_tui_and_slash_command_completion.md),
[T030: Unified Interactive CLI and Plain-Mode Fallback](../done/T030_unified_interactive_cli_and_plain_mode_fallback.md),
[T018: ratatui TUI and Live Approval Flow](../done/T018_ratatui_tui_approval.md),
[T026: Actionable, Redaction-Safe LLM Failure Reporting](T026_actionable_redaction_safe_llm_failure_reporting.md),
[T003: Context Engine with Dynamic Compression](T003_context_engine_with_dynamic_compression_and_session_management.md),
[FT-012: Richer Planner](../done/ft_012_richer_planner.md), and
[FT-017: Managed Process Supervisor](ft_017_managed_process_supervisor.md)

## Summary

Evolve nib's unified interactive CLI into a Codex-inspired coding conversation: a
persistent transcript, capable composer, visible session and execution status,
keyboard-first command discovery, live steering, queued follow-ups, inspectable tool
activity, and explicit approval boundaries.

The full-screen TUI and line-oriented plain/chat renderer must expose the same product
capabilities and operate on the same authoritative session. They may render those
capabilities differently. `nib`, `nib chat`, and `nib tui` remain entry-point or
presentation choices rather than different agents.

The TUI presentation of this contract is a **ledgered coding conversation**: two fixed
status rows, a typed activity transcript, a capable composer, and human-input docks
that do not cover the evidence. It is not a log dump in bordered boxes, not a
permanent administration pane, and not a Codex or Claude Code clone.

Codex is a behavioral reference, not a compatibility target. nib owns its command
contract, workload model, safety rules, persistence, and accessibility behavior. This
spec does not require a pixel-identical clone or automatic adoption of every Codex
command.

## Problem Statement

T025, T028, and T030 established one interactive product with shared commands,
current-session-first rendering, session switching, and automatic TUI/plain selection.
The current interaction remains substantially narrower than a mature coding-agent
conversation:

- the TUI composer is single-line and has no draft history, file mention workflow,
  active-turn steering, or queued follow-up model;
- the transcript is a concatenated string dump (`SessionDetail` plus live `[tag]`
  lines) rather than a unified, inspectable representation of plans, tool calls,
  approvals, questions, diffs, failures, and final answers;
- status such as model, reasoning mode, permissions, context budget, worktree, and
  current execution state is not consistently available in both renderers;
- TUI chrome uses titled `Borders::ALL` regions and a three-row help box, while
  approvals open a modal that covers the transcript needed to decide;
- submitting while a worker is running is rejected instead of distinguishing steer
  from queue;
- nib's command vocabulary covers basic configuration and session management but not
  the complete interaction loop of plan, review, diff, compact, copy, permissions,
  status, background work, resume, and fork; and
- plain/chat and TUI parity is asserted at the command layer but is not yet expressed
  as one state-machine and accessibility contract for richer interactions.

Adding these behaviors independently in each renderer would recreate the drift that
T025 closed. The product needs one semantic interaction model before implementation is
split into reviewable tasks.

## Terminology

- **Interactive product:** the session-scoped agent launched by `nib` or `nib chat`.
- **TUI mode:** the full-screen Ratatui presentation selected automatically or with
  `--tui`; `nib tui` remains a compatibility alias.
- **Plain/chat mode:** the line-oriented presentation selected automatically or with
  `--plain`. The `nib chat` spelling does not itself force this mode.
- **Turn:** one user submission and all planning, model, tool, approval, question, and
  reconciliation activity caused by it.
- **Steer:** a user instruction accepted into the currently active turn.
- **Queued follow-up:** a user instruction durably reserved for the next turn, not an
  instruction silently injected into the current one.
- **Local activity:** non-model-authored status such as plan changes, tool execution,
  approvals, errors, compression, and reconciliation.
- **View model:** the presentation-neutral projection of session, run, queue, command
  availability, and effective configuration that both renderers consume.
- **Activity entry:** one typed transcript item (`user`, `assistant`, `plan`, `tool`,
  `approval`, `question`, `compression`, `reconcile`, `cancellation`, or `failure`).

## Codex Reference Model

This feature is informed by the official OpenAI Codex CLI documentation as observed on
2026-08-20:

- [Codex CLI overview](https://developers.openai.com/codex/cli/features)
- [Codex developer and slash commands](https://developers.openai.com/codex/cli/slash-commands)
- [Codex permissions](https://developers.openai.com/codex/permissions)

The reference scope is the Codex terminal/CLI conversation because nib is a terminal
product. ChatGPT web and desktop-specific interaction contracts are outside this spec.

The documented Codex interaction model provides the following useful patterns. These
are reference behaviors; the normative nib requirements begin in the next section.

| Interaction family | Codex reference behavior | Product lesson for nib |
| --- | --- | --- |
| Orientation | Startup and status surfaces show the model, directory, permissions, and context. | A user should always know which session, model, project, worktree, and authority are active. |
| Composer | `/` discovers commands, `@` attaches workspace paths, history keys restore prompts, and `!` starts a local shell command. | Context attachment and command discovery should happen without leaving the conversation. |
| Live steering | While work is active, one action injects instructions into the current turn and another queues work for the next turn. | Steering and follow-up intent need distinct, visible, auditable semantics. |
| Session control | New, resume, rename, fork, compact, archive, and delete actions are available from the conversation. | Session lifecycle belongs inside the interaction surface, with destructive actions clearly separated. |
| Work modes | Model, reasoning, personality, plan, review, and permission controls are available in-session. | Settings that change how a turn executes must be discoverable and confirmed in the transcript/status surface. |
| Execution visibility | Diff, background-process status, stop, copy, and raw-output controls keep work inspectable. | Tool progress should be summarized by default and expandable without hiding authoritative outcomes. |
| Safety | Permission selection and approvals are explicit and remain constrained by sandbox and managed policy. | UI controls must display effective authority and may never broaden a stronger rule. |
| Keyboard-first operation | Command popups, selectors, history, cancellation, and exit are available without a pointer. | Every TUI flow needs a complete keyboard path and every semantic action needs a plain-mode equivalent. |

Codex commands and key bindings are expected to evolve. nib must not scrape or mirror
the Codex command list at runtime. Changes to nib's public interaction contract require
a reviewed spec and compatibility decision.

## Product Principles

1. **One interaction model, two renderers.** Parsing, command metadata, state
   transitions, session effects, workload submission, approvals, and reconciliation
   are presentation-neutral and live in `src/interactive.rs` (plus agent/session
   persistence where required). TUI and plain/chat translate keys, prompts, and
   worker events into that model. A TUI-only reducer must not own command grammar,
   session effects, or run state.
2. **Conversation is primary.** The current session transcript and composer receive
   the primary space. Administration and details appear on demand. Plan state is a
   status summary with the full plan available through `/plan` or a detail layer; it
   is not a permanently visible third column.
3. **Local activity is not assistant speech.** Plans, tool progress, approvals,
   failures, and reconciliation are visibly distinct from user and assistant messages.
4. **Every action is steerable and auditable.** Users can see what is running, what is
   waiting, what is queued, and what will happen after an approval or cancellation.
5. **Effective permissions are truthful.** The UI reports the authority actually in
   force after configuration, AGENTS.md, skill, worktree, sandbox, and managed-policy
   constraints.
6. **Progressive disclosure preserves signal.** The transcript shows concise activity
   summaries; details remain reachable without flooding the conversation.
7. **Terminal capability is not product capability.** A missing full-screen terminal
   changes presentation, not the operations available to the user.
8. **Compatibility is explicit.** Existing `/session`, `/clear`, and exit aliases
   remain supported until a separate migration spec authorizes removal.
9. **Commands are the stable interface.** `/` completion and `/help` are canonical
   discovery. A TUI command palette, if present, is chrome over the same registry
   and must not introduce a second grammar.

## Normative Interaction Contract

### Interface anatomy

Both modes represent the same four logical regions:

1. **Context header** — project root, active session/name, worktree or branch, active
   profile, and whether the session is local, resumed, forked, or read-only.
2. **Transcript** — ordered user messages, assistant output, and clearly typed local
   activity associated with the active session.
3. **Composer** — editable input plus contextual completion for commands, files,
   models, sessions, and other bounded values.
4. **Status surface** — current lifecycle state, model/reasoning configuration,
   effective permission posture, context usage, queued work, plan `i/n` plus current
   step title when a plan exists, and key hints.

Both must derive their content from the same presentation-neutral view model.

#### TUI presentation

The TUI renders those four regions as a ledger, not as three titled boxes around a
string dump:

- Header and status are **two fixed rows** without `Borders::ALL` chrome. Broad or
  `off` approval modes are labeled in text, not color alone.
- The transcript is a bounded list of typed activity entries. Streaming appends to
  one in-progress assistant entry. Tool lifecycle mutates one tool entry (`requested`
  → approval → `running` → terminal). Follow-tail is the default while a turn is
  active; manual scroll is supported and pin-to-tail resumes on submit or an explicit
  jump-to-end action.
- The composer occupies a growing 2–6 row wrapped region with a visible cursor,
  paste, and a truncation hint when the byte cap drops characters.
- `waiting_approval` and `waiting_question` render as a **dock on the current
  tool or question entry**. The header, plan summary, and earlier transcript remain
  visible. A centered modal that `Clear`s over the evidence is not acceptable for
  these states.
- Session switcher, model, permissions, file, and command selectors may use overlays
  or a temporary detail pane. Overlay errors render **on the overlay** that caused
  them. T028 preview-and-confirm and exact-ID reachability remain required; exact-ID
  preview of an already-listed candidate must refresh that candidate's snapshot
  before selection.
- Plan, diff, and inspect/detail views are on-demand layers (precedence item 5), not
  a permanent spine.
- One designed TUI theme with 16-color and `NO_COLOR` fallbacks is in scope. Role
  prefixes (`you`, `tool`, `plan`, `fail`) must remain distinguishable without color.
  User-selectable themes and mouse-only flows are out of scope.

Plain/chat mode may print a compact header, prefix local activity, and expose
selection through numbered prompts. It does not duplicate TUI widgets.

### Interaction states

The UI exposes one primary state derived from authoritative run and interaction state:

| State | Required user-visible behavior | Allowed transitions |
| --- | --- | --- |
| `idle` | Composer accepts a new message or command. | submit, command, resume, exit |
| `planning` | Plan generation and progress are visible; steering may be accepted only through the defined steer path. | running, waiting, reconciling, failed |
| `running` | Streaming assistant and tool activity update in place; the composer remains available for steer or queue actions. | waiting, reconciling, failed |
| `waiting_approval` | The exact action, scope, risk, and available decisions are shown without hiding the current transcript and plan summary. | running, reconciling |
| `waiting_question` | The question and bounded choices or free-form response field are shown without hiding the current transcript. | running, reconciling |
| `reconciling` | Cancellation or completion is not declared terminal until workload state is updated and the worker is joined. | completed, cancelled, failed |
| `completed` | Final answer and verified outcome are visible; queued work may start exactly once. | idle, running |
| `cancelled` | The cancelled run and queue disposition are explicit. | idle |
| `failed` | T026's safe incident report and recovery action are shown without creating assistant content. | idle, retry command |

An approval, question, selector, or detail view is a presentation layer over these
states. It must not create an alternative run state. Late events remain bound to the
session and run that produced them and cannot appear under a newly active session.

T031 originally shipped queue-only live input. T033 now provides exact-run steering:
accepted input is persisted before delivery and applied only at safe agent boundaries.
It does not mutate the composer into an in-flight prompt or interrupt a provider frame.

### Composer and context attachment

- The composer supports bounded multi-line UTF-8 input, paste, cursor movement,
  deletion, wrap, and explicit submit versus newline actions.
- Draft text remains process-local and must never become session history or model
  context before submission.
- Submitted input is immutable audit evidence. Editing a previous message creates an
  explicit fork rather than rewriting the original session.
- Up/Down restores bounded draft history when no selector consumes those keys. A
  searchable history action is available in TUI mode, with a bounded numbered-search
  equivalent in plain mode.
- Typing `/` opens contextual command completion. Typing `@` opens bounded,
  project-scoped path completion without following unsafe links or escaping readable
  roots. Selected paths are attached as structured context, not expanded into an
  unbounded prompt string.
- A local shell shortcut may be provided only by routing through the normal
  `ToolExecutor`, worktree, boundary, approval, redaction, and audit path. It is not a
  direct shell escape and is disabled when no authoritative session or plan can own
  the action.
- Unknown or incomplete command syntax remains a command error and never falls
  through as an agent goal.
- TUI key dispatch treats `KeyEventKind::Repeat` as input on adapters that emit it
  (Windows consoles and enhanced Unix keyboards). Only `Release` is ignored.
- Characters dropped by the composer byte cap produce a visible truncated status.
  Free-form question input uses the same cap.

### Submission, steering, queueing, and cancellation

- When idle, the submit action starts exactly one turn in the active session.
- While a turn is running, the UI distinguishes **steer current turn** from **queue
  next turn**. Their labels and default key hints must not be ambiguous. **Enter
  never steers.** Accidental injection into a live turn is worse than an extra chord.
- A steer instruction is accepted only when the agent loop can bind it to the exact
  active run. It is persisted as user-authored run input before it can affect further
  planning or tool selection.
- A queued follow-up is persisted with its session, ordering, and source before the UI
  acknowledges it. It becomes a normal user message atomically when its turn begins.
- Queued work starts at most once, only after the preceding run reaches a reconciled
  terminal state. Switching sessions, exit, cancellation, or startup recovery must
  show whether queued work is retained, cancelled, or needs confirmation.
- Cancellation first requests cooperative stop, then reconciles the active workload,
  drains interaction channels, and joins or safely hands off the worker before the UI
  reports `cancelled`.
- Repeated interrupt input may request bounded escalation, but it must not skip audit,
  lease release, descendant cleanup, or reconciliation proof.

Default semantic bindings, pending native-adapter confirmation in the first child
task:

| Semantic action | TUI default | Plain/chat default |
| --- | --- | --- |
| Newline | `Ctrl+J` | continuation / editor |
| Idle submit | `Enter` | Enter |
| Queue next (running) | `Enter` | `queue:` prefix or numbered choice |
| Steer current | `Ctrl+S` while an exact worker is active | `steer:` prefix while an exact run is active |
| Cancel run | `Ctrl+C` | `Ctrl+C` |
| Quit | `Ctrl+Q` or `/quit` | `/quit` (`/exit`, `/q`) |
| Inspect/detail | `Esc` then select; `Enter` expands | numbered detail |
| Command completion | `/` | `/` plus numbered choices |

If a supported adapter cannot reliably report `Ctrl+J` or `Ctrl+S`, the child spec
must document fallbacks that preserve these semantic actions. Do not copy Codex
key-for-key.

### Transcript and activity presentation

- User and assistant messages have stable, visually distinguishable roles in both
  color and non-color output.
- Streaming text updates one in-progress assistant block. Token chunks do not become
  separate persisted or visible messages.
- Plan state is summarized by current step and completion counts on the status
  surface, with the full plan available on demand. The TUI must not keep a permanent
  plan rail.
- Each tool call has one lifecycle card or line group: requested, approval state,
  running, and terminal outcome. Bounded command/output detail is expandable.
- Approval and question responses appear as user decisions or typed local events, not
  assistant-authored prose.
- Diffs use syntax-aware TUI rendering when possible and a bounded unified-diff/plain
  representation otherwise.
- Long output is folded by default with explicit truncation markers and a safe detail
  path. Raw terminal selection mode must never change persisted content.
- Failures use T026's typed safe report. Provider messages, credentials, raw request
  bodies, and presentation control sequences are never shown or persisted.
- Context compression is visible as local activity and preserves the raw audit trail;
  the user can inspect when compaction occurred and what context budget remains.
- TUI wrap and follow-tail scroll must use the same width metric as the rendered
  paragraph (unicode display width, including the wrap-trim flag). Character-count
  estimators are not acceptable.

### Commands and controls

One typed registry owns canonical names, aliases, argument schema, summary, mutability,
availability conditions, worker-state restrictions, completion sources, and help text.
Both renderers consume this registry. A TUI command palette, if added, is an optional
presentation of that registry (for example `Ctrl+K` or `:`); it cannot parse or
execute anything the slash grammar does not.

| Capability | Preferred nib command | Required behavior |
| --- | --- | --- |
| Discover | `/help` and `/` completion | Show only supported commands; disabled conditional commands explain why. |
| Inspect status | `/status` | Show session, project/worktree, provider/model/transport, reasoning, permissions, context, plan/run state, and queued count. |
| Select model | `/model` | Use the configured catalog, preserve exact free-form IDs, and confirm the effective selection. |
| Select permissions | `/permissions` | Inspect or choose an allowed approval/execution posture without weakening managed or project constraints. |
| Plan | `/plan [prompt]` | Enter planning behavior or request a plan without silently starting mutation. |
| Review | `/review` and `/diff` | Review authoritative workspace changes and show the exact diff available to the session. |
| Manage context | `/compact` | Request bounded context compression and report the resulting budget without deleting raw history. |
| Manage sessions | `/new`, `/resume`, `/fork`, `/rename` | Create, preview-confirm resume, branch, or name sessions without mutating an original transcript. |
| Compatibility | `/clear`, `/session` | Continue to map to new-session and preview-confirm resume behavior respectively. |
| Copy output | `/copy` | Copy or print the latest completed assistant output, never an in-progress partial block. |
| Background work | `/ps` and `/stop` | Inspect and stop only work owned by the active session through durable process authority. |
| Ecosystem | `/skills`, `/mcp`, `/providers` | Preserve current presentation-neutral management operations and command parity. |
| Exit | `/quit` with `/exit` and `/q` aliases | Exit only after active-run and queued-work disposition is explicit. |

Commands are capability-gated. Service-specific Codex commands do not appear merely
because Codex documents them. Adding apps, plugins, cloud execution, IDE transfer,
memories, goals, imports, themes, pets, feedback upload, or side chats requires an
implemented nib capability and, where material, its own spec.

Command families ship in this dependency order, with unavailable commands absent or
visibly disabled rather than partially functional:

1. `/status` and `/permissions` over a read-only executor/sandbox view.
2. `/new`, `/resume`, `/rename` (keeping `/session` and `/clear` as aliases).
3. `/plan`, `/review`, `/diff`.
4. `/compact` (depends on T003's explicit compact and budget surfaces).
5. `/fork` (depends on additive lineage schema).
6. `/copy`.
7. `/ps` and `/stop` (depend on FT-017 session-owned process authority).

### Permissions and approvals

- The status surface always indicates the effective approval and sandbox posture. A
  broad or `off` mode is visually prominent without relying on color alone.
- `/permissions` shows the selected preset/profile and any stronger managed,
  AGENTS.md, skill, worktree, or platform limit. It cannot claim permissions the
  executor cannot enforce.
- An approval request shows the normalized action, permission class, target scope,
  worktree, network posture, reason for prompting, and available decisions.
- Decisions are explicit and default to deny/cancel. A keystroke intended for a lower
  interaction layer cannot approve an action.
- Session- or plan-scoped grants are offered only when the policy model can express and
  audit their exact bounds. There is no generic time-based "approve everything" action.
- Plain/chat prompts contain the same information and choices as the TUI dock.
- Every decision flows through the existing `ToolExecutor` and authoritative session
  audit; renderer state is never execution authority.

### Modal precedence and keyboard behavior

Only one interaction layer consumes a key or submitted line. The semantic precedence
is:

1. approval;
2. agent question;
3. destructive confirmation;
4. model, permissions, session, file, or command selector;
5. detail/diff view;
6. completion menu;
7. composer.

Input is routed through one precedence reducer over the shared interaction model, not
an ad-hoc `draw_loop` chain. Default bindings follow the table in Submission,
steering, queueing, and cancellation. Commands remain the stable interface. A help
overlay lists active bindings, and future key customization must map to semantic
actions rather than bypassing this precedence. Bindings must remain testable on
Linux, macOS, and Windows terminal adapters.

Because line-oriented plain terminals cannot distinguish delayed paste/type-ahead
from a later consumer, an approval or question response retains modal ownership until
the user submits a separate empty delimiter line. Non-empty lines before that
delimiter are rejected under the modal and can never become commands, queued turns,
or goals; EOF before the delimiter fails the response closed.

### Plain/chat and TUI parity

- Every command and state transition in this spec has a plain/chat interaction path
  and a TUI path.
- The TUI may use popups, panes, styled spans, and direct key bindings. Plain/chat mode
  uses numbered selectors, explicit textual prompts, bounded prefixes, and commands.
- Features that depend on clipboard, raw scrollback, or advanced key events must offer
  a plain textual fallback or report the missing presentation capability without
  disabling the underlying session operation.
- Redirected and `NO_COLOR` output remains plain, ordered, and free of ANSI/control
  sequences. It is human-readable output, not a new machine protocol.
- Resuming the same session in another presentation produces the same persisted
  transcript, plan, tool audit, queued-work state, and effective configuration.

## Scope

- Define a presentation-neutral interaction state machine and view model shared by
  plain/chat and TUI modes.
- Upgrade the composer with multi-line editing, bounded history, contextual command
  and path completion, and explicit submit/newline semantics.
- Add auditable exact-run active-turn steering and durable queued follow-ups.
- Add status, permission, plan, review/diff, compact, session fork/resume/new/rename,
  copy, and background-work command families over existing runtime capabilities.
- Redesign transcript activity into typed, bounded, inspectable entries while
  preserving current-session-first layout and exact session/run binding.
- Present the TUI as the four-region ledger described above, including approval and
  question docks, overlay-local errors, unicode-width follow-tail, and Repeat-key
  input.
- Preserve and extend approval, question, cancellation, terminal restoration,
  redaction, and reconciliation behavior.
- Provide complete semantic parity between the two renderers and update user-facing
  documentation.

## Non-Goals

- Pixel, color, animation, wording, or key-for-key replication of Codex.
- Depending on Codex internals, dynamically mirroring its command list, or promising
  compatibility with undocumented Codex behavior.
- Adding a web, desktop, IDE, or mobile UI, a file tree, or an embedded editor.
- Implementing Codex service-only features such as cloud execution, apps/plugins,
  memory generation, account usage, imports, feedback upload, or terminal pets.
- Replacing nib's profile-scoped session workload, structured plan, worktree,
  `ToolExecutor`, sandbox, approval, or reconciliation authority.
- A TUI-owned interaction state machine, a second command grammar, or a permanently
  visible plan/administration spine.
- User-selectable themes, mouse-only flows, or starting delivery with theming.
- Changing `nib run` into an interactive command or changing its automation contract.
- Allowing multiple foreground agent turns to mutate one session concurrently.
- Persisting unsent drafts or silently executing queued work after ambiguous recovery.
- Emulating steer by mutating composer text or the in-flight model request before an
  interruptible exact-run boundary exists.
- Making provider-specific transport changes; T021/T022/T023 continue to own those
  contracts.

## Workload and Safety Invariants

- Each displayed and persisted interaction is bound to one profile, session, and,
  where applicable, run and plan identity.
- Draft, completion, overlay, scrolling, and expanded-detail state are presentation
  state and cannot authorize execution.
- Submitted, steered, and queued user input is persisted before it can affect model or
  tool behavior.
- One user action creates at most one command effect, user input record, worker, tool
  decision, or cancellation request.
- Session switching is rejected while a foreground run owns the session. Forking does
  not mutate the source transcript.
- Tool shortcuts and review commands cannot bypass classification, approval,
  worktrees, sandbox boundaries, redaction, audit, or plan gates.
- Local lifecycle and failure records never masquerade as assistant content.
- Renderer failure cannot resubmit a turn, replay a tool, approve an action, or move
  workload state forward.

## Affected Areas

- `src/interactive.rs` — command registry, semantic actions, interaction/view model,
  contextual completion, precedence reducer, and shared state transitions.
- `src/chat.rs` and `src/console.rs` — plain/chat composer, selectors, streaming,
  steering/queue controls, and textual detail views.
- `src/tui/` — split the current single-file TUI into renderer modules over the shared
  model (`mod.rs` launch/preflight/restore, state/key mapping, view, composer,
  worker protocol). No parallel execution authority.
- `src/agent/` — exact-run steering intake, queued-turn handoff, cancellation, and
  event correlation where the existing loop lacks those contracts.
- `src/session/` — additive persisted steering/queue/session-name/fork evidence and
  bounded projections, if required by the final task design.
- `src/tools/`, `src/sandbox/`, and process supervision — read-only presentation APIs
  for effective permissions and session-owned background work; no parallel execution
  authority.
- `src/context/` — visible context budget and explicit compact requests without
  deleting raw history.
- `docs/user/guide.md`, `README.md`, architecture/project-structure references, and
  interactive release notes.
- Parser, reducer, persistence, Ratatui `TestBackend`, child-process, pseudo-terminal,
  and cross-platform terminal tests.

## Implementation Plan

Because this feature crosses composer, session, agent-loop, permission, and renderer
boundaries, implementation must be split into development task specs before code
changes begin:

1. Specify the shared interaction state machine, typed view model, command metadata,
   compatibility aliases, default key semantics, queue-only live-input gate, and the
   remaining persistence/permission-field decisions. This child task is what may move
   FT-019 from backlog to development.
2. Implement the composer, history, contextual completion, typed transcript, and
   header/status rendering without steering or persistence schema changes. This is the
   first user-visible TUI change. Fold in TUI defects that the current string-dump
   surface already has: session-switcher overlay errors, exact-ID snapshot refresh,
   unicode-width follow-tail, `KeyEventKind::Repeat`, composer cursor/wrap, and
   render-path `expect` panics that skip worker cancel/join.
3. Specify and implement durable queued follow-ups, including cancellation and crash
   recovery. Add exact-run steering only after the agent loop can bind it; until then
   the steer action stays absent or disabled.
4. Add status, permissions, plan, review/diff, compact, copy, background-work, and
   expanded session commands in the dependency order under Commands and controls.
5. Complete TUI and plain/chat rendering, accessibility, terminal restoration, and
   parity tests for each slice. Do not start slices with theming, mouse, or a plan
   rail.
6. Update user documentation, run two-stage review, and reconcile FT-019 only after
   every child task and validation gate is complete.

No implementation may start under this backlog spec alone. The first development task
must resolve the open decisions below and define its exact persistence changes.

## Acceptance Criteria

- [x] One presentation-neutral state machine owns interactive command, turn,
      approval, question, steering, queue, cancellation, and reconciliation effects.
- [x] TUI and plain/chat expose the same supported commands, argument semantics,
      availability rules, effective session, and persisted outcomes.
- [x] The active interface shows project/session identity, run state, model/reasoning,
      effective permissions, context status, plan `i/n` when present, and queued-work
      count.
- [x] The TUI renders header and status as fixed rows, the transcript as typed
      activity entries, and the composer as a wrapped multi-line editor; it does not
      keep a permanent plan or session-administration pane.
- [x] The composer supports bounded multi-line input, paste, editing, draft history,
      command completion, and safe project-path attachment.
- [x] Unknown or incomplete commands never execute as goals, and conditional commands
      explain why they are unavailable.
- [x] Idle submit, current-turn steer, and next-turn queue are distinct actions with
      deterministic, auditable, exactly-once behavior. Enter never steers. Steer is
      available only while an exact session/run-bound foreground worker is active.
- [x] Queued work survives only under its documented persistence and recovery policy;
      cancellation, exit, and session switching always show its disposition.
- [x] User, assistant, plan, tool, approval, question, compression, reconciliation,
      cancellation, and failure entries are visibly and structurally distinct.
- [x] Streaming updates one bounded in-progress entry, while completed messages and
      tool outcomes match authoritative persisted state.
- [x] TUI `waiting_approval` and `waiting_question` keep the current transcript and
      plan summary visible. Selector and switcher errors render on the overlay that
      caused them.
- [x] `/status`, `/model`, `/permissions`, `/plan`, `/review`, `/diff`, `/compact`,
      `/new`, `/resume`, `/fork`, `/rename`, `/copy`, `/ps`, and `/stop` meet their
      contracts or are explicitly capability-gated.
- [x] Existing `/session`, `/clear`, `/providers`, `/skills`, `/mcp`, `/help`, `/quit`,
      `/exit`, and `/q` behavior remains compatible.
- [x] `/` remains canonical command discovery; any command palette uses the same
      registry.
- [x] Approval and permission UIs display effective scope and cannot weaken configured,
      managed, AGENTS.md, skill, worktree, sandbox, or platform constraints.
- [x] Modal precedence guarantees one consumer for every key or submitted line.
- [x] Switching, resuming, forking, renderer failure, late events, and worker shutdown
      cannot mix sessions or duplicate execution.
- [x] T026 failure semantics are used consistently and operational failures never
      become assistant messages.
- [x] Narrow terminals, resize, Unicode, large transcripts, long tool output,
      redirected input/output, `TERM=dumb`, and `NO_COLOR` remain bounded and usable.
- [x] User documentation includes a parity matrix, command reference, state model,
      keyboard help, approval behavior, recovery behavior, and migration aliases.
- [x] All child specs complete independent spec-compliance and code-quality/security
      review with no unresolved blocking findings.
- [x] `task docs:check`, `task check`, `task test`, `task check:all-targets`,
      `task coverage`, `task build`, `task smoke:interactive`, native terminal gates,
      and `git diff --check` pass on the exact completion revision.

## Validation Gates

- Registry tests prove every command and alias has unique, bounded, parseable metadata
  and consistent help/completion behavior in both renderers.
- Pure reducer tests cover every state transition, modal precedence, invalid action,
  stale session/run event, duplicate input, and cancellation race.
- Session tests cover exact-run steering, queue ordering and recovery, fork lineage,
  name bounds, context compression evidence, and legacy-record compatibility.
- Deterministic Mock agent tests prove steer-versus-queue timing, exactly-once worker
  creation, tool non-authorization on invalid input, and reconciled cancellation.
- Ratatui `TestBackend` tests cover wide/narrow layouts, resize, transcript roles,
  streaming updates, status, approval/question docks that leave transcript visible,
  overlay-local errors, exact-ID snapshot refresh, diff/detail views, completion,
  Repeat-key dispatch, unicode-width follow-tail, and non-color distinctions.
- Plain/chat child-process tests cover the same semantic scenarios through numbered
  prompts and redirected/`NO_COLOR` output.
- Linux pseudo-terminal smoke covers composer editing, command/file completion,
  status/permissions, approval, question, steering, queueing, cancellation, resume,
  fork, diff/review, clean exit, and terminal restoration.
- Native macOS and Windows jobs exercise their real terminal adapters and prove that
  unsupported presentation capabilities degrade without dropping product operations.
- Security tests prove file mention and shell shortcuts stay within the existing
  scope, approval, redaction, worktree, and audit boundaries.
- Canonical Task and documentation gates listed in the acceptance criteria pass.

## Risks and Mitigations

- **Scope too broad for one patch:** This is an umbrella feature. Require granular
  development specs and dependency-ordered delivery before implementation.
- **Renderer drift:** Keep effects and availability in one semantic registry and run a
  generated parity matrix against both renderers. Do not grow a TUI-only state
  machine.
- **Ambiguous live input:** Distinguish steer and queue in the action model, persist
  before acknowledgement, bind every event to the exact run, and keep Enter from
  steering. Ship queue-only until steer can bind.
- **Transcript overload:** Use typed summaries, folding, bounded details, and explicit
  truncation while keeping raw audit evidence in the session store.
- **Permission theater:** Read effective policy from the executor and sandbox rather
  than reconstructing it from UI selections.
- **Key conflicts:** Route input through one precedence reducer and keep commands as a
  complete fallback.
- **Approval without context:** Render waiting states as docks on the current entry,
  not as modals that cover the transcript.
- **Terminal incompatibility:** Preserve T030 preflight, plain fallback, idempotent
  restoration, non-color distinctions, and native pseudo-terminal validation.
- **Reference drift:** Treat official Codex docs as dated design input. Revise nib only
  through reviewed specs, not automatic mirroring.

## Product Decisions (2026-08-20)

These decisions are part of this backlog spec. They supersede earlier open wording
where they conflict. They do not authorize implementation.

- The TUI is a ledgered presentation of the four logical regions: two fixed
  header/status rows, typed transcript, wrapped composer, on-demand detail. A
  permanent plan spine is rejected.
- `/` is canonical discovery. A command palette is optional TUI chrome over the same
  registry.
- Enter submits when idle and queues when a turn is running. Enter never steers.
  Steer uses a distinct chord or explicit plain prefix and is admitted only through an
  exact session/run-bound durable channel.
- The first user-visible renderer slice was queue-only; T033 replaces that temporary
  gate with checkpointed exact-run steering while preserving queue behavior.
- Approval and question UI in the TUI is a dock on the current activity entry.
- Session-switcher and selector failures are overlay-local. Exact-ID preview refreshes
  an already-listed candidate's snapshot.
- Command families ship in the listed dependency order and stay capability-gated on
  T003, T026, and FT-017 as noted.
- One designed TUI theme with fallbacks is enough. User theming and mouse-only
  operation are out of scope for this feature.

## Resolved Development Decisions and Native Qualification

- `Ctrl+J` is the documented TUI newline binding and `Ctrl+S` is the exact-run steer
  binding. Plain mode provides continuation/editor input and `steer: <text>` as the
  product-operation fallbacks. T034 owns the remaining exact native macOS/Windows key
  and restoration evidence; local or cross-target execution cannot close that gate.
- T031 added backward-compatible `queued_follow_ups`, `display_name`, and `forked_from`
  session fields. Queue recovery never auto-starts ambiguous retained work.
- `/status` and `/permissions` consume the ToolExecutor's shared effective-posture
  projection: configured approval preset, execution provider/profile, network posture,
  validated session worktree identity, and direct/broader-mode warnings. They do not
  reconstruct or broaden policy in either renderer.
- `/status` and `/permissions` shipped through the shared registry and reducer; T032
  subsequently completed `/compact`, `/ps`, and `/stop`, and T033 completed exact-run
  steering.

## Rollout Notes

Ship capability slices behind the shared registry so an unavailable command is absent
or visibly disabled rather than partially functional. Preserve existing aliases and
session formats throughout the rollout. Release notes must distinguish new semantic
actions from key-binding changes and must document the plain-mode equivalent for every
TUI interaction.

FT-019 moved from backlog to development after T031 recorded exact scope, acceptance
criteria, affected areas, persistence decisions, validation gates, and dependency
ownership. It moves to done only after every normative interaction is implemented or
explicitly removed through a reviewed spec amendment. T032 binds `/compact`, `/ps`, and
`/stop` to their runtime owners; T033 owns exact-run steering.

## Implementation Reconciliation (2026-08-21)

T031 completed the first child slice and moved to `docs/specs/done/`. This umbrella
stays in development.

Closed on this revision:

- Bounded in-process TUI draft history: `Up`/`Down` restore prior submissions when
  completion and other overlays are closed, stash the current draft, and drop the
  oldest entry after 50 stored submissions.
- User-guide TUI versus plain command/state/keyboard parity matrix, including draft
  history, queue, steer-unavailable, cancel, quit, completion, session switch, and
  approval presentation.

Closed after T031 on this revision:

- `@` path completion remains project-scoped. Submitting `@path` stores a structured
  `PathAttachment` on the user message and injects bounded file contents into the
  attached-project-paths context section. The user text keeps the mention and does not
  expand file contents into an unbounded prompt string. Escapes, symlinks, and dot
  paths fail closed.
- Queued TUI follow-ups are now claimed only after a gated worker thread and async
  runtime report startup readiness. Startup failure leaves the FIFO item persisted and
  records its retained disposition without persisting raw error text; activation failure
  restores the same item idempotently at the queue head. Deterministic tests cover
  retained failure audit, FIFO claim, and exactly-once restoration.
- Persisted ledger projection now maps user, assistant, tool, and system roles
  explicitly; unsupported legacy roles remain visible as bounded System diagnostics.
  Authoritative session events project into typed plan, approval, question, compression,
  reconciliation, cancellation, failure, and System entries. Projection uses timestamps
  where present and stable source/index tie-breakers when timestamps are equal or absent;
  it does not infer chronology that persistence did not record. Tool lifecycle events
  defer to `ToolCallRecord`, and event/failure projection exposes only allowlisted
  structural evidence rather than raw arguments, output, or provider text. Deterministic
  tests cover ordering, redaction, deduplication, and legacy JSON roundtrip stability.
- The TUI composer now applies Unicode-safe Delete at the caret and consumes bracketed
  paste events as bounded multi-line input. Paste normalizes CRLF and lone CR line
  endings, omits unsafe controls, truncates only on UTF-8 boundaries, and renders an
  explicit status when content is omitted. Terminal restoration disables bracketed
  paste before leaving the alternate screen on normal, error, and guard-drop paths.
  Reachable model/session overlay invariant checks now render bounded recoverable UI
  errors instead of panicking. Deterministic tests cover editing, paste safety and
  truncation, modal fallback rendering, and terminal control restoration.
- `/plan <prompt>` now emits a typed presentation-neutral plan-mode run effect in both
  renderers. Plain and TUI workers pass `mode = "plan"` to the existing agent loop, while
  ordinary and queued submissions remain execute mode; the resulting structured plan is
  persisted unapproved and reconciled as `plan_ready` without approval or tool execution.
  `/review` and `/diff` now prefer the active session's validated durable managed-worktree
  ownership, revalidate it after inspection, fail closed on stale, replaced, or escaping
  ownership, and fall back to the project root only when no session ownership exists. Git
  inspection uses the existing bounded managed runner and UTF-8-safe output truncation.
  Deterministic tests cover typed effects, both renderer runtimes, project fallback,
  owned-worktree selection, bounds, and ownership rejection.
- `/status` and `/permissions` now render one presentation-neutral effective execution
  posture resolved through the same ToolExecutor instruction-tightening and platform
  sandbox route used for tool execution. Configured approval is labeled separately;
  effective provider/profile/network, mutation plan and managed-worktree gates,
  instruction fail-closed state, direct/off warnings, and the stronger per-action
  AGENTS/skill/tool-policy limits are explicit without relying on color. `/status` also
  reports the resolved LLM transport and bounded approximate persisted context usage.
  Permission changes recompute this posture and never claim to override stronger
  controls. Deterministic tests cover network tightening, invalid-directive fail-closed,
  off warnings, platform routing, transport/context bounds, and control-safe output.
- Agent-loop cancellation and pre-turn non-LLM reconciliation stay in typed session
  events rather than synthetic assistant content. When a process dies after durable
  tool completion but before its private provider continuation completes, recovery
  appends one bounded structured assistant-role boundary containing only
  `provider_continuation_interrupted`. This keeps the next user turn role-valid without
  replaying the tool or persisting provider continuation state. Cancellation summaries
  omit `last_message` so a gateway cannot echo the interrupted user prompt. Recovery
  precedes even an already-cancelled restart's generic reconciliation, preventing that
  event from hiding an older open continuation. A recovery-persistence failure after
  run admission still records the matching exact-run `local_error` terminal and cannot
  reach cancellation/provider effects. Deterministic unit, injected-failure, and
  process-kill/restart regressions cover these boundaries.
- Plain and TUI approval prompts now consume one normalized `ApprovalContext` produced
  after ToolExecutor resolves scope, risk, effective network posture, and worktree
  requirements. It presents an allowlisted actionable operation summary, permission/risk,
  target scope, truthful pending managed-worktree disposition, reason, and bounded grant
  semantics without rendering raw argument JSON. Terminal commands, patch target names,
  merge/management targets, configured secrets and their percent/Base64 variants,
  controls, and oversized values are redacted and strictly bounded; plan approvals use
  the same contextual path with plan identity, step count, and a bounded goal summary.
  Legacy `ApprovalHandler` implementations remain compatible through a default contextual
  method, and deterministic tests preserve decisions and transcript-visible TUI docks.
  Public `approval_required` and `tool_started` stream/session lifecycle evidence now
  carries only typed identity/status fields; raw arguments remain solely in the
  authoritative redacted tool-call audit rather than being duplicated into UI events.
- Agent runs now bind a validated caller-supplied or private generated 32-hex run ID to
  provider request scope, additive run summaries, and exactly one persisted start and
  terminal lifecycle record. Replayed IDs fail before prompt, approval, or tool effects.
  TUI workers stamp the same private ID on stream envelopes and accept live events only
  when both session and exact active run match, isolating late same-session events while
  keeping steer unavailable.
- One presentation-neutral reducer now owns approval, question, destructive
  confirmation, selector/detail, completion, and composer precedence, plus typed
  command, queue, idle-turn, cancel, quit, stale-event, no-op, and bounded error
  reductions. TUI keys and exact-run stream envelopes map into that reducer before
  renderer-specific handling. Plain submitted lines, approval/question answers,
  numbered selectors, session confirmation, and command completion use the same
  consumer selection and submission classification. Command errors and modal input
  cannot fall through as goals or lower-priority actions. Pure table-driven and
  renderer-adapter tests cover precedence, exactly-one consumption, stale session/run
  events, invalid-action recovery, command non-fallthrough, and plain/TUI mapping.
- Transcript navigation now uses an explicit presentation-only row viewport over the
  same Unicode-width wrapping projection rendered by the TUI. Follow-tail is the
  default; PageUp/PageDown clamp by deterministic visible pages, streamed activity
  preserves an unpinned top row, and submit or Ctrl+End explicitly resumes following.
  The visible status/footer reports both pin state and key hints. Draft history remains
  process-local, keeps the existing 50-entry/consecutive-deduplication policy, and now
  uses one bounded Unicode/control-safe search model. Ctrl+R or `/history [query]`
  opens the keyboard-only TUI selector, while plain mode provides the same bounded
  results as a numbered select-and-confirm flow. Pure, TestBackend, and plain tests
  cover empty/Unicode/bounded/no-match history, restoration/eviction/precedence,
  narrow wrapping, resize/clamping, append while unpinned, and explicit repinning.
- Interactive release qualification is now split between the deterministic
  `task test:interactive` semantic suite and `task smoke:interactive:binary`, which
  drives the already-built optimized binary through redirected input and a real Linux
  pseudo-terminal using Mock only. The fixture removes ambient provider credentials,
  keeps its redaction sentinel on an inactive provider, isolates HOME/config/project/
  session state, applies hard timeouts and cleanup, and checks
  terminal-mode, alternate-screen, and bracketed-paste restoration. The PTY cases
  exercise multiline Unicode paste/edit/Delete, command and project-path completion,
  status/permissions/history, transcript-visible approval/question docks, exact-run
  steering versus queue, cancellation reconciliation, resume/fork, review/diff, manual
  scrolling/repinning, resize/narrow rendering, `TERM=dumb`, `NO_COLOR`, and clean
  exit. Authoritative ledgers prove attachment, queue, cancellation, and private-data
  invariants where Ratatui differential writes are not a stable textual oracle.
  `task test:interactive` passed 135 focused tests, the optimized `task build` passed,
  and `task smoke:interactive:binary` passed on Linux. This is not native macOS or
  Windows terminal evidence; those gates remain open.
- Run lifecycle activity remains typed but no longer renders the private exact run ID,
  and the approval dock honors `NO_COLOR` while retaining bold/text signaling. Focused
  tests and the Linux smoke assert that persisted run IDs, inactive-provider sentinel
  credentials, raw argument bodies, and unsafe pasted controls do not reach renderer
  output or the message ledger. The provider-neutral request scope and enclosing
  request also redact exact session/run values from their public `Debug` surfaces.
- `ask_question` now has one registry-owned closed schema: a required non-empty bounded
  question and at most 20 bounded non-empty options. The executor validates that schema
  before opening a modal, the agent loop accepts it only as the sole tool in its batch,
  and both renderers receive control-safe bounded text. Invalid arguments cannot create
  a pending interaction or authorize another tool.
- TUI shutdown requests agent cancellation first, resolves pending approval/question
  dependencies, drains matching lifecycle events, and joins the worker for at most five
  seconds before returning control to terminal restoration. Cancellation wins a
  same-tick race with approval denial. Deterministic responsive and unresponsive worker
  tests plus the optimized Linux PTY smoke cover the bounded exit path.
- T032 makes `/compact` an exact-session maintenance run over T003 compression without
  manufacturing a user or assistant message. It preserves raw history, uses an exact
  compare-and-swap publication, and records matching run and compression evidence.
- T032 makes `/ps` and `/stop <task-id>` use a bounded allowlisted projection and an
  atomic session-owner check over FT-017's durable workload authority. Foreign and
  missing task IDs are indistinguishable at the interactive boundary; `/stop` without
  an ID is read-only and always explains the exact-ID follow-up.
- T033 adds a bounded exact-session/exact-run steering channel. Plain `steer:` and TUI
  `Ctrl+S` persist ordered user input before delivery, apply it at safe provider/tool
  boundaries, supersede unapproved plans, suppress uncommitted provider tool proposals,
  and preserve Enter/`queue:` as next-turn input. Typed activity omits private run IDs,
  and terminal/channel races persist explicit delivery-failure evidence.

Combined local validation on 2026-08-26 passed `task check`,
`task check:all-targets`, `task coverage` at 85.71% (82,695 / 96,482), the 16/16 runtime
end-to-end suite, the locked optimized build, and the redirected/PTY release smoke.
These are Linux/local results; they do not satisfy native macOS/Windows terminal or
final independent-review gates.

Historical items remaining before Done at this stage:

- The optional local shell shortcut is not a Done blocker unless a later reviewed
  decision makes it required.
- Native macOS and Windows terminal jobs, final umbrella two-stage review, and the
  remaining umbrella acceptance items. T033's independent two-stage review is complete.

## Final Interaction and Native-Harness Reconciliation (2026-08-27)

Fresh umbrella review found and the implementation repaired four locally actionable
boundaries. Active TUI submission now parses slash commands before live-run queueing,
so `/quit`, unknown commands, and idle-only commands cannot become later agent goals.
Plain queue chaining prepares the next runtime/worker dependencies before atomically
claiming the FIFO head, starts every successful follow-up in order, and retains later
work after cancellation or startup failure. Header/status chrome now renders the
profile captured at startup and revalidates the current session worktree instead of
trusting historical tool-call labels.

The command registry now owns typed argument schemas, mutability, availability,
live-worker policy, completion candidates, aliases, usage, and help metadata. One
explicit interaction lifecycle derives idle/planning/running/waiting/reconciling and
terminal presentation, while the shared reducer owns approval, question, destructive
confirmation, command, queue, steering, stale-event, and reconciled-terminal semantics.
Both renderers map presentation input into those reductions. TUI cancellation reports
the drained authoritative reconciliation after worker join rather than unconditionally
labeling a racing completion as cancelled, and failed/nonterminal runs do not claim
queued work.

T034 adds the remaining local native-terminal harness implementation: Darwin-portable
Unix PTY supervision, bounded Windows ConPTY input and console-mode evidence, a
Mock-only Windows optimized-binary smoke, native post-build CI wiring, Task entry
points, static contracts, and technical documentation. Linux locally passed the
optimized binary smoke; exact hosted macOS/Windows execution, a clean completion
revision, and aggregate canonical evidence remain open.

## Final Independent Review Evidence (2026-08-27)

A fresh independent spec-compliance review passed the locally implemented FT019
contract and all 8/8 T031 criteria. A separate fresh code-quality/security review
passed after repairs for empty control-prefix fallthrough, typed failure/FIFO
retention, terminal-aware quit reporting, joined-terminal status projection, and
plain modal type-ahead. Plain approval/question replies now remain pending under modal
ownership until a separate empty delimiter line arrives; delayed non-empty lines are
rejected for any duration and cannot reach command, queue, or goal handling. The
focused regression delays surplus input beyond the removed timing heuristic.

T034 also passed independent implementation-scope spec review and a separate
code-quality/security/portability review after its Unix ledger privacy scan and
Windows fixture cleanup were made fail-closed. FT019 is therefore locally 21/22: only
the exact clean-revision aggregate and native terminal criterion remains open.

The final native Linux qualification also caught an eager-input defect outside the
modal reducer: one-shot `nib run --yes` started the console reader even when no prompt
was requested, so the release watchdog's background process group stopped on
`SIGTTIN`. The bounded console broker is now lazy and starts exactly once on the first
real input request. Its six tests are included in `task test:interactive`, bringing the
focused gate to 160 tests, and the unchanged bounded `task smoke:interactive` now
passes the optimized Linux PTY and redirected matrix end to end.

The final local canonical matrix passed `task test` (943 library tests, 86 binary
tests, and every integration/doctest target), all 39 installer tests, all 5
documentation-integrity tests, `task check`, `task check:all-targets`, `task coverage`
at 85.98% (85,650 / 99,615), `task build`, and `task smoke:interactive`. This remains
local dirty-worktree Linux evidence; FT019 stays at 21/22 until the exact clean revision
passes the native macOS and Windows qualification owned by T034.

Hosted run `33649455308` passed the complete macOS job and advanced Linux through the
ordinary suite, coverage, and exact release qualification. Windows reached 972 other
passing library tests but the exact-run compression steering regression exhausted its
five-second first-tool observation window under late-suite load. The test still requires
the exact `run_terminal` completion, Compression transition, durable steering intake,
continuation abandonment, and terminal answer; only its test-local positive-progress
windows are now 15 seconds on Windows for the two observations while other platforms
retain five seconds and terminal completion retains ten seconds. The first observation
also fails explicitly if the stream closes before the scripted tool's terminal event.
`task test:interactive` continues to select the full exact-run steering family serially.
The exact hosted native criterion remains open pending the replacement revision.

After the Windows-only observation allowance and closed-stream diagnostic, the complete
`task test:interactive` gate passed the 16 exact-steering tests and every focused
interaction, TUI, console, chat, CLI, and selector-contract group. Final `task verify`
passed 1,061 library tests, 86 CLI tests, all integration suites, and doctests; final
`task coverage` passed at 85.86% (101,992/118,788). Independent spec-compliance and
code-quality/security reviews found no remaining blocker. Native hosted acceptance
remains open until the replacement exact revision passes Windows as well as macOS.


## Final Closure Evidence (2026-09-02)

This section supersedes earlier remaining-plan, current-risk, completion-state, and
native-evidence notes only where they described validation gates now executed. PR
[#25](https://github.com/skills-yaml/nib/pull/25) exact implementation run
[33683995100](https://github.com/skills-yaml/nib/actions/runs/33683995100)
passed the Validate, macOS Tests, and Windows Tests jobs for head
`c3b88564da4f6f654a8618e4fa544b353ece86f5` at clean merge checkout
`0479b72ad3d11fd7221632f042736b8489b6443b`. The matrix passed the complete
serial suites, Linux coverage at 85.87 percent (102,061/118,862), all native
all-target gates, exact release-binary qualification, and the Linux, macOS, and
Windows platform smokes.

The exact optimized binary hashes were
`e9b56b4c2b527ab04bd4e40932c83a632ae5bd5931010dee6152012b421e4276`
(Linux), `e7bbf6ea23d87a3e00b1447fc7880f2c93e6c67a27239f0068bcb599d18fb739`
(macOS), and
`e9250200aa0b06188e3e05d062ccd39115eb98311d0dc9b691cfdc5e9a324423`
(Windows). Local `task verify` also passed 1,062 library tests, 86 CLI tests,
every integration suite, and doctests during this reconciliation. All previously
open acceptance and validation items in this file are satisfied for its shipped
scope by this final matrix and the prior evidence recorded above.

T034 closes on the same native release binaries and platform smokes before this
umbrella transition, satisfying the FT-019 dependency order.
