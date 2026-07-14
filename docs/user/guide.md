# nib End-User Guide

This guide covers installing, configuring, using, and maintaining `nib`.

---

## 1. Getting Started

### Prerequisites
- **Linux/macOS:** A terminal with `curl` installed.
- **Windows:** PowerShell.
- An API Key from an LLM provider (e.g., OpenAI, Anthropic, Gemini, Grok).

### Installation
You can install `nib` using the provided installation scripts. This will download the binary and place it in your local binary path (typically `~/.local/bin` on Unix systems and `%USERPROFILE%\.local\bin` on Windows).

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

### Authentication
Run the authentication command to configure your API keys:
```bash
nib auth
```
This interactive prompt saves your configuration to `~/.config/nib/config.toml`. 

### Global Configuration (`config.toml`)
You can manually edit `~/.config/nib/config.toml` to fine-tune `nib`'s behavior:
- **LLM Settings:** Switch providers or specify default models.
- **Execution Limits:** Set max iterations for the agent loop to prevent run-away tasks.
- **Auto-Approve:** Toggle strict human-in-the-loop approvals for dangerous commands.

### Custom Skills
`nib` dynamically reads custom `.md` skills from `~/.config/nib/skills/` or local project `.nib/skills/`. To add new behaviors, place a `SKILL.md` file in one of these directories.

---

## 3. Core Features

- **Agent Loop:** The execution loop transitions through states: `Idle` -> `BuildContext` -> `InspectLlm` -> `UpdateMemory` -> `ToolExecute`. The Tool Executor routes all actions securely.
- **Sandboxing (Codex):** By default, file modifications outside the project root and potentially dangerous system commands are either blocked or run inside a restricted boundary (via `bwrap` on Linux).
- **TUI Approvals:** For destructive actions (e.g., running an unverified terminal command), `nib` pauses execution and presents a Terminal User Interface (TUI) for you to review and approve/reject the action.
- **MCP Server:** `nib` can act as a backend. Using `nib mcp-server`, IDEs or other MCP clients can access `nib`'s local tool capabilities over JSON-RPC.

---

## 4. Usage

### Interactive Chat
Start a conversational session with `nib` inside any project directory:
```bash
nib chat
```
Available slash commands in chat:
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
This starts a JSON-RPC 2.0 server over `stdio`.

---

## 5. Maintenance & Troubleshooting

### Updating `nib`
To update `nib` to the latest version, re-run the installation script. It will automatically overwrite the binary with the newest release.

### Diagnostics (`nib doctor`)
If you experience issues, run the built-in diagnostic tool to verify API keys, sandbox capabilities, and connectivity:
```bash
nib doctor
```

### Uninstallation
To uninstall `nib`, delete the binary and the configuration directory:

**Unix:**
```bash
rm ~/.local/bin/nib
rm -rf ~/.config/nib
```

**Windows (PowerShell):**
```powershell
Remove-Item -Path "$env:USERPROFILE\.local\bin\nib.exe" -Force
Remove-Item -Recurse -Force "$env:USERPROFILE\.config\nib"
```
