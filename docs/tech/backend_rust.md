# Backend Rust

This document details the conventions and implementation standards for the pure Rust core of nib.

## Pure Rust Core

nib is fully implemented in Rust. There is no Python runtime, `uv`, or subprocess bridge used for execution or agent reasoning.
The `nib` binary contains everything required: CLI, TUI, configuration, tool execution, sandboxing, and LLM communication.

### Key Libraries and Frameworks

- **CLI Shell**: `clap` is used for command-line argument parsing (e.g., `nib run`, `nib chat`).
- **TUI**: `ratatui` with `crossterm` is used for the terminal user interface, providing views for session history, live agent runs, and approval modals.
- **Async Runtime**: `tokio` is the standard asynchronous runtime.
- **Configuration**: Managed via `toml` (and `serde`). Config is strictly kept in `.nib/config.toml`.
- **HTTP / LLMs**: `reqwest` (with `rustls`) is used for all LLM API calls. `async-trait` is used for the `LlmClient` abstraction.
- **Data Serialization**: `serde` and `serde_json` for LLM APIs, profile-scoped session storage, daemon state, and tool calling formats.
- **Error Handling**: `thiserror` for robust error modeling.
- **Sandboxing**: Git worktrees isolate mutations. The hybrid provider adds `bwrap`
  OS isolation on usable Linux hosts and otherwise falls back to direct execution in
  the worktree; the strict `bwrap` provider fails closed.

### Project Structure (Rust specific)

- `src/main.rs`: Entry point. Sets up logging and invokes the `clap` CLI router.
- `src/auth.rs`, `src/chat.rs`, `src/run.rs`, and command modules: thin CLI command logic.
- `src/agent/`: The core agent loop and planning abstractions.
- `src/llm/`: The `LlmClient` traits and provider implementations (OpenAI, Anthropic,
  Gemini, Grok, OpenRouter, Meta, Mock).
- `src/tools/`: The `ToolRegistry` and `ToolExecutor`, encompassing permissions, approval gates, and actual implementations.
- `src/sandbox/`: Hybrid sandbox execution logic (bwrap invocation, boundaries, named profiles).
- `src/session/`: indexed session, plan, event, memory, and profile-scoped persistence logic.
- `src/config/`: Configuration definitions.

### Build and Testing

- **Taskfile**: All development tasks are orchestrated via `task`.
- **Quality Gates**: `task check` validates installers, formatting, Clippy warnings,
  compilation, and tests. `task docs:check` validates links/spec state, and
  `task coverage` enforces runtime line coverage.
- **Unit and Fixture Tests**: CI runs against `MockLlmClient` to prevent flakiness and network dependencies.

### OpenAI-Compatible Transport Contract

OpenAI-compatible providers resolve an explicit `chat_completions` or `responses` API
mode before network I/O. `LlmClient` receives a structured request, and streaming
separates sanitized projected events from a private validated completed-turn envelope.
Only the completed envelope can authorize tool execution. Responses continuations are
byte/item bounded, bound to provider/model/session/run, redacted under `Debug`, and
kept in memory only; session persistence contains provider-neutral audit evidence.

The runtime does not infer capabilities from model names, silently disable reasoning,
or retry a rejected request with different API semantics. Responses uses `store: false`
for nib's local-first state contract, which is distinct from provider-side retention
policy or Zero Data Retention eligibility.
