# FT-005: LLM Streaming & TUI Live View

**Status:** Backlog
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
