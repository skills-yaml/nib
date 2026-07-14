# nib

nib is a command-line AI coding assistant that executes tasks in a sandboxed environment. It integrates directly with your local workspace to plan, implement, and verify code changes.

## Features

- **Local Sessions:** Maintains conversation and tool call history locally in `./.nib/sessions/`.
- **Pluggable LLMs:** Supports OpenAI, Anthropic, Gemini, Grok, and local providers.
- **Sandboxed Execution:** Restricts file modifications outside the project root and runs dangerous commands inside a restricted boundary (`bwrap` on Linux).
- **Human-in-the-Loop:** Pauses for approval before executing destructive actions.
- **MCP Integration:** Can act as an MCP (Model Context Protocol) server for external IDEs or clients.

## Installation

### Pre-built Binaries

**Linux / macOS**

```bash
# Install latest stable release
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh

# Install development release
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib NIB_CHANNEL=development sh
```

The script downloads the binary and installs it to `~/.local/bin`. Ensure this directory is in your `PATH`.

**Windows (PowerShell)**

```powershell
& ([scriptblock]::Create((irm "https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.ps1"))) -Channel prod -Repo skills-yaml/nib -AddToPath
```

### Build from Source

Requirements:
- Rust toolchain (stable)
- Task (https://taskfile.dev)

```bash
git clone https://github.com/skills-yaml/nib.git
cd nib
task build
./target/release/nib --help
```

## Quick Start

1. **Authenticate:** Configure your API keys.
   ```bash
   nib auth
   ```

2. **Run a single task:**
   ```bash
   nib run "Refactor the authentication logic in src/auth.rs"
   ```

3. **Start an interactive session:**
   ```bash
   nib chat
   ```

   Inside the chat, you can use slash commands:
   - `/help` - Show available commands
   - `/model` - Switch the active LLM model
   - `/quit` - Exit the session

## Documentation

- **[End-User Guide](docs/user/guide.md)** — Detailed instructions on configuration and features.
- **[Technical Docs](docs/tech/)** — Architecture, CI, and project structure references.
- **[Specs](docs/specs/)** — Feature and product specifications.