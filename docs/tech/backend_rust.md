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
- **Data Serialization**: `serde` and `serde_json` for interacting with LLM APIs, session storage (`.nib/sessions/*.json`), and tool calling formats.
- **Error Handling**: `thiserror` for robust error modeling.
- **Sandboxing**: OS-level isolation is achieved through direct execution of `bwrap`. Task isolation is handled via Git worktrees.

### Project Structure (Rust specific)

- `src/main.rs`: Entry point. Sets up logging and invokes the `clap` CLI router.
- `src/cli/`: Logic for individual commands (auth, chat, run, doctor, etc.).
- `src/agent/`: The core agent loop and planning abstractions.
- `src/llm/`: The `LlmClient` traits and provider implementations (OpenAI, Anthropic, Gemini, Grok, OpenRouter, Mock).
- `src/tools/`: The `ToolRegistry` and `ToolExecutor`, encompassing permissions, approval gates, and actual implementations.
- `src/sandbox/`: Hybrid sandbox execution logic (bwrap invocation, boundaries, named profiles).
- `src/session/`: `SessionStore` logic for persisting `.nib/sessions/*.json`.
- `src/config/`: Configuration definitions.

### Build and Testing

- **Taskfile**: All development tasks are orchestrated via `task`.
- **Quality Gates**: `task check` runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo check`, and `cargo test`.
- **Unit and Fixture Tests**: CI runs against `MockLlmClient` to prevent flakiness and network dependencies.
