# nib

The AI agent that owns your coding workload and ships.

## Description

nib is a specialized AI agent for coding and workload management. It maintains a persistent, accurate model of your projects and work items while driving actual implementation through disciplined execution loops (planning, delegation, verification, and reconciliation).

Unlike general chat agents that forget context between sessions, nib treats the backlog as a first-class, long-lived artifact. It can break down goals, choose what to work on next, implement (or orchestrate) changes using the best available sub-agents and tools, verify the results, and keep the workload state truthful and visible.

Primary users:
- Individual developers and technical founders who want a reliable daily partner for both "what should I do?" and "do the work".
- Small teams that want consistent, reviewable agent-driven progress on code projects.
- Power users already living in rich agent environments (Grok subagents, MCPs, and similar tools) who want a workload-native orchestrator on top.

Key capabilities (MVP focus):
- Local-first sessions (`./.nib/sessions/`) for conversation + tool call history.
- LLM-driven agent loop with pluggable providers (Grok, OpenAI, Anthropic, etc.).
- Rigorous execution: gated ToolExecutor, hybrid bwrap sandboxing, git worktrees, plan/approval gates.
- Excellent CLI (`nib chat`, `nib run`, `nib auth`) + TUI visibility and human steering.
- Strong integration with Git, GitHub (MCP), skills, and existing agent platforms.

## Vision

The most trustworthy, persistent agent for solo and small-team software development — one that keeps your workload real, surfaces the right decisions, and reliably turns intent into shipped, verified code across days and weeks.

## Project Structure

- `docs/specs/` – Product and feature specifications (foundation, feature, and task docs following revized-style conventions).
- `docs/tech/` – Architecture and process references.
- Hybrid implementation: **Rust CLI** (primary entrypoint, `src/`) + **Python core** (agent loop, LLM, tools in `src/nib/`).
- See `docs/tech/backend_python.md`, `docs/tech/project_structure.md`, and `docs/tech/ci.md` for conventions and layout.

## Installation

### Stable Release (Recommended)

**Linux / macOS**

```bash
# Install latest stable (prod channel)
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh

# Development channel (bleeding edge)
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib NIB_CHANNEL=development sh
```

The script downloads the appropriate binary for your platform and installs it to `~/.local/bin`.

**Windows (PowerShell)**

```powershell
# Stable
irm https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.ps1 | iex

# With parameters (recommended)
& ([scriptblock]::Create((irm "https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.ps1"))) `
  -Channel prod -Repo skills-yaml/nib -AddToPath
```

After installation, ensure `~/.local/bin` (or the equivalent) is in your `PATH`.

To update later, simply re-run the same install command (it will overwrite with the latest for the chosen channel).

### From Source (Developers)

```bash
git clone https://github.com/skills-yaml/nib.git
cd nib

# Quick dev build + checks
task dev

# Or just build the release binary
task build
./target/release/nib --help
```

Requires:
- Rust toolchain (stable)
- Task (https://taskfile.dev)
- uv (for Python core parts)

### First-Time Setup

After installing `nib`:

```bash
nib auth          # Configure LLM providers (OpenAI, Anthropic, Grok, etc.)
nib chat          # Start an interactive session
nib run "your goal"
```

See `scripts/first-time-setup.sh` for an automated first-run helper.

## Quick Start

```bash
nib chat
# Inside chat:
#   /model                # List & switch models for the current provider
#   /help
#   /quit
```

## Documentation

- [docs/specs/foundation/product.md](docs/specs/foundation/product.md) — Product foundation.
- `docs/specs/done/` — Completed feature specs (FT-003 Hybrid Sandboxing, FT-004 LLM + Agent Loop).
- `docs/tech/` — Technical references:
  - `architecture.md`
  - `ci.md` (builds, releases, installation)
  - `project_structure.md`
  - `permissions.md`, etc.
- `agents/memory/` — Durable project decisions and facts.