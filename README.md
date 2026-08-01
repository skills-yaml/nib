# nib

nib is a command-line AI coding assistant that plans, executes, and reconciles work in local Git worktrees with explicit approval and audit records.

## Features

- **Local Sessions:** Stores profile-scoped history under `./.nib/profiles/<id>/sessions/`.
- **Pluggable LLMs:** Includes OpenAI Responses and Chat Completions transports plus configured adapters for Anthropic, Gemini, Grok, OpenRouter, Meta, and Mock.
- **Layered Execution:** Isolates mutations in Git worktrees; the default hybrid provider uses Linux `bwrap` when usable and otherwise runs directly in the worktree.
- **Human-in-the-Loop:** Manual mode prompts for plans and risky actions; explicit deny policies remain authoritative.
- **MCP Integration:** Can act as an MCP (Model Context Protocol) server for external IDEs or clients.
- **Durable Work:** Background commands and scheduled wakes have inspectable, cancellable, lease-fenced records.
- **Supervised Delegation:** Foreground subagents use an independent cleanup supervisor;
  production delegation requires a usable Linux bwrap PID namespace. Windows Job
  Object and macOS process-group backends remain non-production native test mechanisms
  until their cleanup authority is isolated from managed workers.
- **Persistent Memory:** Approval-gated environment and user facts carry across sessions without replacing the raw audit trail.
- **Verified Self-Updates:** Official release builds can run `nib update`; ordinary
  user-facing launches notify when their rolling channel has a different build.

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

After the first updater-capable release is installed, update within its existing
production or development channel with:

```bash
nib update
```

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
   cd /path/to/project
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
   - `/skills` - Manage installed skills
   - `/mcp` - Manage MCP servers
   - `/quit` - Exit the session

Mutating work remains on a `nib/session/*` worktree branch until you review and merge
it. `nib run --yes` bypasses interactive plan and tool prompts and should be limited to
already trusted environments.

## Documentation

- **[End-User Guide](docs/user/guide.md)** — Detailed instructions on configuration and features.
- **[Technical Docs](docs/tech/)** — Architecture, CI, and project structure references.
- **[Specs](docs/specs/)** — Feature and product specifications.
