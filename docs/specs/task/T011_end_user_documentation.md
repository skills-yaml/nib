# T011: End-User Documentation Creation

## Problem Statement

Now that the core runtime, tool executor, execution limits, context engine, sandbox integration, MCP gateway, and CI/CD release pipelines have been established, `nib` requires comprehensive documentation for the end user. End users need clear instructions on how to install, maintain, configure, and operate the tool to utilize all of its AI-driven features effectively.

## Goals

- Provide a seamless onboarding experience for new users.
- Document the entire lifecycle: Installation, Configuration, Usage, Maintenance, and Uninstallation.
- Detail the features and capabilities of `nib`, including its skills system, sandbox features, and MCP integration.

## Scope

- Create an `docs/user/` directory (or consolidate within a comprehensive `README.md`) dedicated to end-user facing manuals.
- Create an **Installation Guide**: How to install via the pre-built installer scripts (Linux/macOS `install.sh`, Windows `install.ps1`).
- Create a **Configuration & Setup Guide**: How to run `nib auth`, configure the `~/.config/nib/config.toml` file, set up LLM API keys (OpenAI, Anthropic, Gemini, etc.), and configure global skills.
- Create a **Usage Guide**: How to invoke `nib`, interact with it in a project workspace, use the TUI for approvals, and understand the core commands (e.g., `nib chat`, `nib mcp-server`).
- Create a **Features Overview**: Explanation of the sandbox (`codex`), the context engine, the tool executor, and how the agent seamlessly handles task orchestration.
- Create a **Maintenance Guide**: How to update to the latest release channel (prod vs development) and how to run `nib doctor` for troubleshooting.

## Out of Scope

- Technical architecture documentation (already covered under `docs/tech/`).
- API documentation or deep code-level documentation (e.g., rustdoc).

## Design / Structure

The end-user documentation will be organized as follows (either as separate markdown files under `docs/user/` or clear, anchored sections in the `README.md`):

1. **Getting Started**
   - Prerequisites
   - Quick Install (curl / PowerShell scripts)
   - Authentication & First-time setup (`nib auth`)

2. **Core Concepts & Features**
   - The Agent Loop & Tool Executor
   - Safe execution via Sandbox (Codex)
   - Skills System (`SKILL.md`)
   - TUI Approvals (Human-in-the-loop)

3. **Usage**
   - Interactive Chat (`nib chat` / `nib run`)
   - Serving via MCP (`nib mcp-server`)
   - Troubleshooting and Diagnostics (`nib doctor`)

4. **Maintenance**
   - Updating `nib`
   - Managing Configurations (`config.toml`)
   - Uninstalling

## Exit Criteria

- The end-user documentation is fully written and committed.
- The root `README.md` clearly links to or contains this end-user documentation.
- The documentation accurately reflects the current state of the implemented features, including the `install.sh` scripts and the `mcp-server` command.
