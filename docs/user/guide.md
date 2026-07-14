# nib End-User Guide

Welcome to **nib**, the specialized AI agent for coding and workload management. Unlike standard chat assistants, `nib` owns the implementation and execution of your tasks. It leverages a rigorous execution loop, sandboxed environment, robust tools, and the MCP (Model Context Protocol) to integrate directly into your workflow.

This guide will walk you through installing, configuring, using, and maintaining `nib`.

---

## 1. Getting Started

### Prerequisites
- **Linux/macOS:** A terminal with `curl` installed.
- **Windows:** PowerShell.
- An API Key from an LLM provider (e.g., OpenAI, Anthropic, Gemini, Grok).

### Installation
You can install `nib` using our pre-built installers, which download the correct binary for your system and place it in your local binary path (typically `~/.local/bin` on Unix systems and `%USERPROFILE%\.local\bin` on Windows).

**Linux / macOS:**
```bash
# Install the latest stable release
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh
```

**Windows (PowerShell):**
```powershell
& ([scriptblock]::Create((irm "https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.ps1"))) -Channel prod -Repo skills-yaml/nib -AddToPath
```

> Ensure that your local bin directory is added to your system's `PATH`.

---

## 2. Configuration & Setup

Once installed, you must configure `nib` to communicate with your preferred LLM provider.

### Authentication
Run the authentication command:
```bash
nib auth
```
This interactive prompt will help you set up your API keys. The configurations are saved to `~/.config/nib/config.toml`. 

### Global Configuration (`config.toml`)
You can manually edit `~/.config/nib/config.toml` to fine-tune `nib`'s behavior:
- **LLM Settings:** Switch providers or specify default models.
- **Execution Limits:** Set max iterations for the agent loop to prevent run-away tasks.
- **Auto-Approve:** Toggle strict human-in-the-loop approvals for dangerous commands.

### Global Skills
`nib` dynamically reads custom `.md` skills from `~/.config/nib/skills/` or local project `.nib/skills/`. To add new behaviors, simply drop a `SKILL.md` file into one of these directories.

---

## 3. Core Concepts & Features

Understanding how `nib` operates will help you get the most out of it.

- **The Agent Loop & Tool Executor:** When you issue a command, `nib` transitions through a strict state machine: `Idle` -> `BuildContext` -> `InspectLlm` -> `UpdateMemory` -> `ToolExecute`. The Tool Executor routes all actions safely.
- **Safe Sandboxing (Codex):** By default, file modifications outside the project root and dangerous system commands are either blocked or run inside a restricted boundary (via `bwrap` on Linux).
- **Human-in-the-Loop (TUI Approvals):** For potentially destructive actions (like running an unverified terminal command), `nib` pauses execution and presents a Ratatui-based Terminal User Interface (TUI) for you to review, approve, or reject the action.
- **MCP Gateway Integration:** `nib` isn't just a standalone CLI; it can serve as a backend. Using `nib mcp-server`, IDEs or other MCP clients can securely access `nib`'s local tool capabilities over JSON-RPC.

---

## 4. Usage

### Interactive Chat
Start a persistent conversational session with `nib` inside any project directory:
```bash
nib chat
```
Inside the chat, you can type your tasks directly, or use slash commands:
- `/help` - Show available commands.
- `/model` - Switch the active LLM model.
- `/quit` - Exit the session.

### Direct Execution
For one-off tasks without entering the interactive shell, use:
```bash
nib run "Refactor the authentication logic in src/auth.rs to use async/await"
```

### Running the MCP Server
To allow external agents, clients, or IDEs to invoke `nib`'s internal tools:
```bash
nib mcp-server
```
This starts a standard JSON-RPC 2.0 loop over `stdio`.

---

## 5. Maintenance & Troubleshooting

### Updating `nib`
To update `nib` to the latest version, simply re-run the installation script. It will automatically overwrite the binary with the newest release.

### Diagnostics (`nib doctor`)
If you experience issues, `nib` includes a built-in diagnostic tool. Running it will verify:
- API key presence.
- Sandbox (bwrap) capabilities.
- LLM Provider connectivity.
- Skills discoverability.

Run the diagnostics with:
```bash
nib doctor
```

### Uninstallation
To uninstall `nib`, delete the binary and the configuration directory:
```bash
# Unix
rm ~/.local/bin/nib
rm -rf ~/.config/nib

# Windows
Remove-Item -Path "$env:USERPROFILE\.local\bin\nib.exe" -Force
Remove-Item -Recurse -Force "$env:USERPROFILE\.config\nib"
```
