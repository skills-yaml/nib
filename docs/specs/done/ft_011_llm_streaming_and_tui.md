# FT-011: LLM Streaming & TUI Live View

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Implement streaming support for all `LlmClient` providers and integrate it into the Ratatui-based TUI, allowing users to see the LLM reasoning (tokens) and tool results generated in real-time.

## Problem Statement
At the feature baseline, the agent loop executed synchronously. The user issued a goal
and then waited for the full LLM response before seeing output, making slower tasks
appear stuck. The reconciliation below records the shipped streaming path.

## Goals
- Add a `.stream()` method (or equivalent) to the `LlmClient` trait.
- Update `OpenAiCompatClient`, `AnthropicClient`, `GeminiClient` to yield chunks.
- Update `AgentLoop` to handle streaming responses, parsing tool calls incrementally.
- Build a live-updating TUI that renders text and active tool execution statuses dynamically.

## Scope
- Update `LlmClient` trait to support streaming tokens and tool calls.
- Implement streaming for all current LLM clients.
- Modify `AgentLoop` to process streams instead of blocking for full completion.
- Integrate UI rendering (Ratatui) to display the live stream and tool executions.

## Acceptance Criteria
- When a task is started, the TUI immediately shows streamed text.
- Tool executions are displayed in the TUI as they are parsed from the stream.
- The agent loop can gracefully complete the response and proceed to the next step when the stream ends.
- `task check` passes without issues.
- All related unit tests pass.

## Affected Areas
- `src/llm/mod.rs` (LlmClient trait)
- `src/llm/openai.rs` (OpenAiCompatClient implementation)
- `src/llm/anthropic.rs` (AnthropicClient implementation)
- `src/llm/gemini.rs` (GeminiClient implementation)
- `src/agent/loop.rs` (AgentLoop logic)
- `src/tui/mod.rs`.

## Validation Gates
- `task check`
- `task test`
- Manual verification of streaming text and tool calls in TUI by running the app.

## Reopened Audit (2026-07-15)

Scope: complete Gemini tool-call streaming, emit live tool lifecycle/result events,
avoid a blocking pre-stream planner phase, and add deterministic parser/TUI tests.

Affected areas: `src/llm/`, `src/agent/`, `src/tui/`, and streaming tests.

Validation gates: provider parser fixtures, agent/TUI event tests, manual TUI smoke,
`task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Stream text and incremental tool calls from every provider through the agent loop and
render bounded lifecycle/tool/terminal events in the TUI.

### Acceptance Criteria

- [x] `LlmClient::stream` exists with provider-specific OpenAI, Anthropic, and Gemini implementations.
- [x] Fragmented tool arguments are accumulated deterministically.
- [x] Planner and execution turns consume streams and emit content/lifecycle events.
- [x] TUI renders text, tool calls, approvals, questions, terminal output, completion, reconciliation, and end events.
- [x] Stream and live-output byte limits fail safely.
- [x] Manual raw-TUI streaming/lifecycle smoke is recorded; provider wire behavior is fixture-verified.

### Affected Areas

`src/llm/`, `src/agent/`, `src/llm/types.rs`, `src/tui/mod.rs`, and streaming tests.

### Implementation Evidence

Provider `stream` implementations parse SSE/JSON fragments; `ToolCallAccumulator` and
`AgentLoop` assemble calls; `LiveOutput` renders a bounded tail.

### Validation Evidence

`stream_posts_stream_flag_and_accumulates_text_and_tools`,
`stream_consumes_named_sse_events_and_partial_tool_json`, and
`stream_consumes_sse_text_and_function_calls` cover providers. TUI lifecycle/bound tests cover rendering.
Manual raw-PTY runs on 2026-07-15 covered live lifecycle rendering, plan
approval/denial, questions, detail view, cancellation, reconciliation, and clean exit.

### Validation Gates

- [x] Provider parser/HTTP fixtures and deterministic TUI event tests exist.
- [x] Manual raw-TUI smoke plus protocol-real local HTTP/SSE provider fixtures.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Paid-provider credential smoke remains an optional operator check, not a completion
gate. Deterministic tests exercise each provider's real HTTP/SSE protocol without
external credentials; the PTY smoke covers terminal timing on Linux.

## Final Quality Review Remediation (2026-07-15)

### Scope

Stop provider HTTP/SSE reader tasks immediately when the model-event receiver is
dropped or the first explicit provider terminal event is delivered, so agent
cancellation/completion releases network work and buffers promptly. Preserve distinct
Gemini function calls across streamed response chunks.

### Acceptance Criteria

- [x] OpenAI-compatible, Anthropic, and Gemini stream producers stop on channel closure.
- [x] Error/end sends also treat receiver closure as terminal.
- [x] Deterministic dropped-receiver fixtures prove the server connection closes early.
- [x] The first parsed or sentinel terminal event ends the producer immediately, without
  waiting for the provider socket to close or emitting post-terminal events.
- [x] Intentionally open-socket fixtures prove terminal completion for all three providers.
- [x] Gemini function calls split across chunks retain distinct stream-level identities,
  including provider call IDs when present.

### Affected Areas

`src/llm/openai.rs`, `src/llm/anthropic.rs`, `src/llm/gemini.rs`, and provider fixtures.

### Validation Gates

Focused provider stream tests, `task test`, `task check`, and `task coverage`.
