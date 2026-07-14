# T012: Toolset Expansion and Capability Gap Bridging

## Problem Statement

Currently, `nib` has a strong foundational toolset focused strictly on local file manipulation, searching, and synchronous terminal execution (`read_file`, `list_directory`, `grep`, `apply_patch`, `run_terminal`). However, when compared to advanced agent environments (like Antigravity), `nib` lacks higher-order capabilities. To serve as a truly autonomous orchestrator, `nib` must bridge the gap by supporting subagent delegation, web research, background task scheduling, and rich human-in-the-loop interactions.

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
