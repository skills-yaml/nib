# T011: End-User Documentation Creation

**Status:** Done

## Historical Problem Statement (Proposal-Time)

Now that the core runtime, tool executor, execution limits, context engine, sandbox integration, MCP gateway, and CI/CD release pipelines have been established, `nib` requires comprehensive documentation for the end user. End users need clear instructions on how to install, maintain, configure, and operate the tool to utilize all of its AI-driven features effectively.

## Goals

- Provide a seamless onboarding experience for new users.
- Document the entire lifecycle: Installation, Configuration, Usage, Maintenance, and Uninstallation.
- Detail the features and capabilities of `nib`, including its skills system, sandbox features, and MCP integration.

## Delivered Scope

- Maintain the end-user manual in `docs/user/guide.md` and link it from `README.md`.
- Document installation through the Linux/macOS and Windows installer scripts.
- Document `nib auth`, project `.nib/config.toml`, provider credentials, and skills.
- Document project use, TUI approvals, and commands such as `nib chat` and
  `nib mcp-server`.
- Explain hybrid/bwrap sandboxing, context, tool execution, and orchestration.
- Document updates, uninstallation, and `nib doctor` troubleshooting.

## Out of Scope

- Technical architecture documentation (already covered under `docs/tech/`).
- API documentation or deep code-level documentation (e.g., rustdoc).

## Design / Structure

The end-user documentation is organized in `docs/user/guide.md` with entry links and
installation highlights in `README.md`:

1. **Getting Started**
   - Prerequisites
   - Quick Install (curl / PowerShell scripts)
   - Authentication & First-time setup (`nib auth`)

2. **Core Concepts & Features**
   - The Agent Loop & Tool Executor
   - Safe execution via hybrid worktrees, policy, and bwrap where available
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

## Reopened Audit (2026-07-15)

Scope: correct configuration/storage paths, commands, chat/TUI behavior, sandbox
terminology, skills/MCP setup, maintenance, and uninstall guidance against the final
locally verified runtime.

Affected areas: `README.md`, `docs/user/`, installers, and documentation links.

Acceptance criteria: the guide documents every shipped command and trust boundary,
all examples match CLI help and current storage paths, and no obsolete runtime is
presented as current behavior.

Validation gates: CLI-help comparison, installer/config path review,
`task docs:check`, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Document installation, profile-scoped state, authentication/configuration, run/chat,
TUI approvals/questions, skills, stdio MCP, durable tasks, memory, safety, doctor,
updates, and uninstall behavior exactly as shipped.

### Acceptance Criteria

- [x] `docs/user/guide.md` covers the supported install and lifecycle workflows.
- [x] Root `README.md` links to the user guide and current technical references.
- [x] Storage paths and trust boundaries match profile JSON, hybrid sandboxing, stdio MCP, and external gateway adapters.
- [x] Installer examples match the checksummed release scripts and channels.
- [x] CLI help and raw-terminal behavior were compared against the operator guide.
- [x] The 2026-07-16 local documentation and repository gates are green.

### Affected Areas

`README.md`, `docs/user/guide.md`, installer scripts, CLI command definitions,
`docs/tech/`, and documentation/installer tests.

### Implementation Evidence

- `docs/user/guide.md` is the durable operator guide; `README.md` provides the quick path.
- `scripts/install.sh`, `scripts/install.ps1`, and `scripts/first-time-setup.sh` are the documented installers.

### Validation Evidence

- `tests/installers.rs` covers checksum success/failure and Unix/PowerShell defaults.
- `tests/docs_integrity.rs::internal_markdown_links_resolve` validates local links.
- CLI behavior is covered by `src/run.rs`, `src/chat.rs`, `src/config_cmd.rs`,
  `src/mcp_cmd.rs`, and their tests.
- `task docs:check` passed all five documentation integrity tests on 2026-07-15.
- Manual release-binary smoke on 2026-07-15 covered help/version output, healthy and
  failing doctor, skill install/list/remove, MCP initialize/list/call/error bounds,
  durable terminal cancellation/reconciliation, scheduled wake delivery, and raw-PTY
  TUI detail, cancellation, approval, and question flows.

### Validation Gates

- [x] Final CLI-help and manual raw-PTY TUI comparison.
- [x] `task docs:check` after reconciliation (5 passed on 2026-07-15).
- [x] `task check`.
- [x] `task test`.

### Documented Boundaries

The guide does not imply that nib hosts Telegram/Slack/Discord transports or supports
MCP HTTP/OAuth. Non-Unix terminal behavior remains platform-specific validation.

### Current Validation Addendum (2026-07-16)

The guide reconciliation corrected the MCP removal example, documented inbound and
outbound MCP lifecycle and platform limits, and documented exact-owned worktree cleanup
and abrupt-owner behavior. `task docs:check` passed 5/5, `task check` and independent
`task test` passed 795 tests, and `task coverage` reported 83.90 percent
(55,083/65,656). Linux
release-binary and raw-PTY smoke passed. Windows and macOS runtimes were unavailable,
so their behavior remains an unexecuted platform gate in the owning development specs.
