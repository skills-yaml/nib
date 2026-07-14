# FT-005: LLM Streaming & TUI Live View

**Status:** Development
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Implement streaming support for all `LlmClient` providers and integrate it into the Ratatui-based TUI, allowing users to see the LLM reasoning (tokens) and tool results generated in real-time.

## Problem Statement
Currently, the agent loop executes synchronously. The user issues a goal and then waits for the full LLM response to complete before seeing any output. For complex tasks or slower models, this creates a poor developer experience where the agent appears "stuck."

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
- `cargo fmt && task check` pass without issues.
- All related unit tests pass.

## Affected Areas
- `src/llm/mod.rs` (LlmClient trait)
- `src/llm/openai.rs` (OpenAiCompatClient implementation)
- `src/llm/anthropic.rs` (AnthropicClient implementation)
- `src/llm/gemini.rs` (GeminiClient implementation)
- `src/agent/loop.rs` (AgentLoop logic)
- `src/tui/app.rs` or related UI components.

## Validation Gates
- `task check`
- `task test`
- Manual verification of streaming text and tool calls in TUI by running the app.
