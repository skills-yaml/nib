# T012: Toolset Expansion and Capability Gap Bridging

**Status:** Done

## Historical Problem Statement (Proposal-Time)

At proposal time, `nib` had a foundational local toolset but lacked higher-order
delegation, web, background scheduling, and rich human-interaction capabilities. The
reconciliation below records the shipped implementations.

## Goals

- Expand the `ToolExecutor` and `ToolMetadata` registry to include native tools for web, orchestration, and interactive UI.
- Maintain strict safety boundaries and `nib`'s rigorous execution loops while adding these capabilities.
- Ensure all new tools are seamlessly exposed over the existing MCP Server implementation.

## Scope

The following tool capabilities will be introduced into `nib`:

1. **Subagent Orchestration**
   - `invoke_subagent`: Launch a secondary `nib` loop (or external model) in an isolated context/worktree.
   - `manage_subagents`: List or terminate running subagents.
   - `send_message`: Pass instructions or data between the main agent and subagents.

2. **Web Capabilities**
   - `search_web`: Perform web queries (e.g., via DuckDuckGo, Brave Search, or an MCP proxy) to resolve external unknowns.
   - `read_url_content`: Fetch and convert HTML to markdown for documentation parsing without leaving the loop.

3. **Background Task & Time Management**
   - `manage_task`: Fork heavy terminal commands (`run_terminal`) into non-blocking background tasks and poll their status.
   - `schedule`: Set up recurring cron-like timers to wake the agent loop at a later time.

4. **Rich User Interaction (UX)**
   - `ask_question`: Pause the agent loop and render an interactive multi-choice modal in the TUI, allowing the human to clarify intent before proceeding.

## Out of Scope

- **Image / Asset Generation:** Native integration of text-to-image models (e.g., `generate_image`) is deferred to external MCP servers to keep `nib`'s core binary lean.

## Design & Implementation Details

- **Tool Registry Expansion:** Update `src/tools/registry.rs` to register the new tool schemas.
- **Agent Loop Modifications (`src/agent/loop.rs`):**
  - For `ask_question`, the agent must transition to a `WaitingForUserInput` state, rendering the question payload in the TUI, and wait for human response via `stdin` or Ratatui event loops.
  - For `schedule` and `manage_task`, introduce an asynchronous task manager in `src/daemons/` that can inject synthetic messages back into the agent's context when a timer fires or a task completes.
- **Web Execution:** `search_web` and `read_url_content` will utilize `reqwest` for HTTP execution, returning safe, sanitized markdown.
- **Subagent Worktrees:** `invoke_subagent` will heavily leverage the existing `sandbox/` and `git worktree` abstractions to ensure subagents cannot corrupt the main execution state.

## Exit Criteria

- New tools are registered in `src/tools/registry.rs` and exposed in `get_tools_schema()`.
- `cargo test` passes, including new integration tests demonstrating background task parsing.
- TUI correctly intercepts `ask_question` tool calls and returns the human's response to the LLM context.
- MCP Server maps the new tools correctly to external clients.

## Reopened Audit (2026-07-15)

Scope: replace web/question/schedule/task/subagent stubs with complete gated tools,
deliver task/timer events to the loop, and expose the full schemas over MCP.

Affected areas: `src/tools/`, `src/daemons/`, `src/agent/`, `src/tui/`,
`src/integrations/mcp_server.rs`, and tool/E2E tests.

Acceptance criteria: every expanded tool is implemented behind the normal scope,
classification, approval, audit, and reconciliation path; asynchronous results are
delivered back to the originating session; schemas are exposed through MCP.

Validation gates: each expanded tool has success/error/permission coverage, TUI
question flow and MCP schema/call tests, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Add network research, isolated subagents, durable terminal/task/schedule management,
profile memory, and clarification tools under the same registry, approval, audit, and
reconciliation path as the five core tools.

### Acceptance Criteria

- [x] Subagent spawn/manage/message/cancel and verified merge are implemented with linked worktrees and sessions.
- [x] `search_web` and `read_url_content` enforce network approval, URL/DNS/redirect safety, content bounds, and sanitization.
- [x] Background terminal and schedules persist, survive process exit, and deliver observations to the originating session.
- [x] `manage_memory` persists bounded profile environment/user memory through normal tool audit.
- [x] `ask_question` pauses and resumes the same loop through CLI/TUI handlers.
- [x] Expanded schemas are advertised by the stdio MCP server.
- [x] Final aggregate gates are green.

### Affected Areas

`src/tools/`, `src/daemons/`, `src/agent/`, `src/tui/`,
`src/integrations/mcp_server.rs`, and expansion/delegation/durable tests.

### Implementation Evidence

- `src/tools/core.rs` implements web, task, memory, schedule, question, and delegation dispatch.
- `src/daemons/workload.rs` owns durable workers; `src/tools/delegation.rs` owns
  child records/worktrees; `src/agent/loop.rs` owns question and observation reconciliation.

### Validation Evidence

- `src/tools/core.rs`: bounded web parsing/SSRF, memory, terminal, and scoped grep tests.
- `tests/delegation.rs`: ten delegation lifecycle and verified-merge tests.
- `tests/durable_tasks.rs`: cross-process terminal/schedule/redaction tests.
- `src/tui/mod.rs`: question response/cancel and worker-shutdown tests.

### Validation Gates

- [x] Focused expanded-tool success/error/security tests exist.
- [x] TUI question and MCP schema/call paths have deterministic coverage.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Live public-network reliability is intentionally not a deterministic CI dependency;
network parsing and trust-boundary behavior use local fixtures and resolvers.

## Final Quality Review Remediation (2026-07-15)

### Scope

Make background terminal and schedule creation atomic across durable persistence,
in-memory admission, and success audit so the expanded tool surface never reports a
task that cannot be managed.

### Acceptance Criteria

- [x] Background terminal and schedule admission cannot strand orphan prepared records.
- [x] Schedule/session audit is emitted only for a durably admitted task.
- [x] Cross-profile duplicate IDs cannot suppress rollback of the newly prepared task,
  and rollback/terminalization failures remain visible to the caller.
- [x] Listing and startup reconciliation remain memory-bounded when many individually
  valid durable results exist.
- [x] Reconciliation reports use a bounded or streaming interface rather than accumulating
  every reconciled record, and guard/worker compensation failures reach the caller or
  durable audit evidence.
- [x] Count limits remain usable after terminal tasks accumulate.
- [x] Terminal-record retention/eviction is deterministic, bounded, and auditable while
  active records are never removed for capacity.
- [x] Focused expanded-tool rollback and cap tests pass.

### Affected Areas

`src/tools/core.rs`, `src/tools/executor.rs`, `src/agent/loop.rs`,
`src/daemons/task.rs`, `src/daemons/workload.rs`, and executor tests.

### Validation Gates

Focused expanded-tool tests, `task test`, `task check`, and `task coverage`.

## Reopened Audit (2026-07-16)

### Scope

Provide a real stdin-backed `QuestionHandler` for `nib run` and a shared-input handler
for interactive chat so `ask_question` resumes outside the TUI.

### Acceptance Criteria

- [x] `nib run` renders the question/options, accepts a typed or numbered response, and
  resumes the same loop.
- [x] Chat uses its existing input stream for question responses without deadlocking or
  consuming the next command incorrectly.
- [x] Empty/closed input fails clearly and reconciles without side effects.
- [x] Deterministic CLI and chat tests cover question round trips.

### Affected Areas

`src/run.rs`, `src/chat.rs`, console question handling, and CLI tests.

### Implementation Plan

1. Introduce one synchronized console input abstraction shared by prompts and the REPL.
2. Implement approval and question handlers over that input.
3. Wire both handlers into run and chat, and surface unavailable input after
   reconciliation.
4. Cover numbered, free-form, empty, closed, run, and chat paths deterministically.

### Risks

Independent stdin locks can deadlock or consume a later chat command. A single shared
reader serializes all console reads; failed question input ends only the active agent
turn and is recorded before control returns.

### Completion Evidence

`ConsoleInput` is shared by REPL, approval, and question handlers. Run and chat tests
cover successful numbered/free-form answers plus closed input and role-safe
reconciliation. Numeric selection accepts only `1..=len`; numeric free-form responses
remain valid when no options are present.

### Validation Gates

Focused run/chat question tests, `task check`, `task test`, and `task coverage`.
