# Project Structure

nib CLI is implemented entirely in **Rust**. There is no longer a Python core. The agent loop, LLM clients, and tool execution all run in-process in the Rust binary.

See skm's project_structure.md for reference on CI/install layout.

- Rust: `src/main.rs`, top-level command modules, `src/agent/`, `src/llm/`, `src/tools/`, `src/sandbox/`, `src/session/`, and `src/profile/`.

The `nib` command is the compiled Rust binary containing all execution logic.

## High-Level Layout

```text
nib/
├── Cargo.toml
├── Cargo.lock
├── build.rs
├── Taskfile.yml                 # All dev, check, test, and build commands
├── AGENTS.md
├── README.md
├── .gitignore
├── .github/workflows/           # ci.yml + release.yml (skm style)
├── scripts/                     # installers, release transaction, and validation helpers
├── docs/
│   └── ... (specs, tech)
├── src/
│   ├── main.rs                  # Rust CLI entry (clap)
│   ├── lib.rs                   # Public runtime module surface
│   ├── auth.rs                  # Provider authentication command
│   ├── chat.rs                  # Interactive chat command
│   ├── config_cmd.rs            # Config show/edit/validate command
│   ├── console.rs               # Shared approval/question console input
│   ├── context_cmd.rs           # Context inspection command
│   ├── doctor.rs                # Runtime health checks
│   ├── fs_security.rs           # Shared filesystem identity/link checks
│   ├── interactive.rs           # Shared chat/TUI commands and interaction effects
│   ├── mcp_cmd.rs               # MCP configuration command
│   ├── mcp_test_fixture.rs      # Debug-only MCP subprocess fixture
│   ├── run.rs                   # One-shot agent command
│   ├── skill_cmd.rs             # Skill list/install/remove command
│   ├── task_cmd.rs              # Durable task command
│   ├── updater.rs               # Verified self-update + startup availability checks
│   ├── version.rs               # Build metadata display
│   ├── agent/                   # Loop, planner, and run state
│   ├── config/                  # Configuration schema and persistence
│   ├── context/                 # Context, budgets, compression, docs, skills
│   ├── daemons/                 # Cron, curator, timers, durable workload
│   ├── integrations/            # Gateway, MCP framing/client/server, worktrees
│   ├── llm/                     # Provider clients, factory, stream types, mock
│   ├── profile/                 # Profile resolution and legacy migration
│   ├── sandbox/                 # Execution, process scopes, platform backends
│   ├── session/                 # Sessions, plans, audit, and profile memory
│   ├── tools/                   # Models, registry, gates, built-ins, delegation
│   └── tui/                     # Ratatui interface
└── tests/
```

## Key Directories & Ownership

- `src/agent/`, `src/context/`, and `src/llm/` — Planning, prompt construction, model transport, streaming, and run reconciliation.
- `src/tools/` — Tool contracts, registration, classification, approval/policy gates, implementations, and delegation.
- `src/sandbox/` — Direct/`bwrap` execution, managed process scopes, Windows Job Object support, and owned subagent worktrees.
- `src/fs_security.rs` — Shared filesystem identity and no-link primitives. Security-sensitive persistence and execution code should reuse this module.
- `src/config/`, `src/profile/`, `src/session/`, and `src/daemons/` — Configuration plus profile-scoped session, memory, and durable workload state.
- `src/integrations/` — Normalized gateways, bounded MCP framing, outbound/inbound MCP, and session worktree integration.
- `src/interactive.rs` — Presentation-neutral chat/TUI command grammar, session
  selection, model selection, management effects, and stream-event display mapping.
- `src/main.rs`, top-level command modules, `src/console.rs`, and `src/tui/` — Presentation and dispatch layers. They should stay relatively thin and reuse `src/interactive.rs` for shared capabilities.
- `docs/specs/` — Product truth. Never implement major behavior without a corresponding spec or task plan.
- `docs/tech/` — Engineering conventions. Keep them up to date as the project evolves.

## Package Rules

- Rust binary (`nib`) is the user-facing CLI and the core engine.
- All code lives under `src/` and is compiled with `cargo`.

## What nib Is NOT (for structure decisions)

- Not a microservices platform → no `backend/libs/`, `srv/`, `lambda/`, Firestore, Pub/Sub, etc.
- Not primarily an API server (though it may grow lightweight MCP server or HTTP surfaces later).
- Primary interfaces are excellent **CLI + TUI**, not web UIs.

## Future Growth

If nib evolves further:
- A web dashboard → add a `fe/` directory.
- Background services → consider a `srv/` directory.

The current design keeps the CLI, agent loop, tools, sandbox, persistence, and LLM
clients in one Rust binary.

## References

- `docs/tech/architecture.md` — Base architecture.
- `docs/tech/ci.md` — Build, CI, release, and installation details.
- `docs/tech/backend_rust.md` — Rust core conventions.
- [FT-004 LLM and agent loop spec](../specs/done/ft_004_llm_integration_and_agent_loop.md).
- Central workspace references for patterns.

Update this document whenever the top-level layout changes significantly.
