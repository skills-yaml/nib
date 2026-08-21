# Backend Rust

This document details the conventions and implementation standards for the pure Rust core of nib.

## Pure Rust Core

nib is fully implemented in Rust. There is no Python runtime, `uv`, or subprocess bridge used for execution or agent reasoning.
The `nib` binary contains everything required: CLI, TUI, configuration, tool execution, sandboxing, and LLM communication.

### Key Libraries and Frameworks

- **CLI Shell**: `clap` is used for command-line argument parsing. `nib` launches the
  unified interactive UI, while `nib run` remains the one-shot interface.
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
- `src/chat.rs`: Unified `auto`/`plain`/`tui` interactive launcher and plain renderer.
- `src/auth.rs`, `src/run.rs`, and other command modules: thin CLI command logic.
- `src/agent/`: The core agent loop and planning abstractions.
- `src/llm/`: The `LlmClient` traits and provider implementations (OpenAI, Anthropic,
  Gemini, Grok, OpenRouter, Meta, Mock). Provider wire metadata remains in the Rust
  registry; source-attributed model defaults are embedded from
  `src/llm/default_models.toml`.
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
mode before network I/O. `LlmClient` receives a typed `LlmRequest` (`LlmMessage`,
`ToolDefinition`, `GenerationOptions`) rather than wire-shaped JSON messages. Streaming
separates sanitized projected events from a private validated completed-turn envelope.
Only the completed envelope can authorize tool execution. Responses continuations are
byte/item bounded, bound to provider/model/session/run, redacted under `Debug`, and
kept in memory only; session persistence contains provider-neutral audit evidence.

The runtime does not infer capabilities from model names, silently disable reasoning,
or retry a rejected request with different API semantics. Responses uses `store: false`
for nib's local-first state contract, which is distinct from provider-side retention
policy or Zero Data Retention eligibility.

### LLM Failure Boundary

`LlmClient::complete`, `LlmClient::stream`, and `LlmStream` return the canonical
provider-neutral `LlmError`. Adapters classify only local request state, numeric HTTP
status, and exact allowlisted structural codes. Complete and streaming failures retain
the registered provider, transport, redacted model, phase, retry disposition, optional
HTTP status, and a stable incident class; provider messages and arbitrary response
metadata are never part of the public or durable record.

The agent keeps `AgentRunSummary.outcome` as a stable machine token and stores the
optional structured failure separately in reconciliation evidence. Operational LLM
failures are lifecycle events, not assistant messages, so later context cannot mistake
them for model-authored content. Console, TUI, gateway, delegated, and durable observers
derive their bounded report and recovery action from the typed class instead of parsing
diagnostic text. Internal compatibility messages are redacted and bounded in memory but
are skipped during serialization.

### Provider Model Catalog

The bundled model catalog is a strict, versioned TOML data file rather than a Rust
allowlist. Startup validates that every registered provider has one unique ordered
model list, that its default appears in that list, and that source and verification
metadata are present. A malformed bundled catalog fails immediately during registry
access so releases cannot silently ship incomplete defaults.

`llm.providers.<id>.models` is an optional per-project replacement for bundled picker
suggestions. Omission inherits the bundled list; an explicit list, including an empty
list, replaces it. The independently configured `model` stays free-form and is added
to the effective picker when absent. Auth and catalog updates preserve user overrides
and existing selected models.

### Live Provider Qualification

`tests/llm_live.rs` is an ignored, credential-gated integration target for mutable
provider compatibility evidence. Its catalog clients discover account-visible models,
its dry-run planner fixes the request/attempt/output-token denominator before generation,
and every generation scenario uses the production registry, configuration validation,
factory, adapter, stream terminal handling, and private tool-continuation path. Direct
providers qualify the live catalog; OpenRouter qualifies only the approval-gated
exact-ID fixture under `tests/fixtures/llm_live/`.

The typed `LlmRequest` carries an optional `max_output_tokens` ceiling so the harness can
bound paid output through every production transport. Ordinary runtime callers retain
their previous provider default when the field is omitted. Live JSON and Markdown
reports are bounded, pseudonymize private model IDs, scan configured sensitive values,
and publish without overwriting an existing report.
