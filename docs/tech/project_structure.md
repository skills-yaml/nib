# Project Structure

nib CLI is implemented entirely in **Rust**. There is no longer a Python core. The agent loop, LLM clients, and tool execution all run in-process in the Rust binary.

See skm's project_structure.md for reference on CI/install layout.

- Rust: `src/main.rs`, `src/cli/`, `src/agent/`, `src/llm/`, `src/tools/`, `src/sandbox/`, `src/session/`, etc.

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
├── scripts/                     # install.sh, install.ps1, first-time-setup.sh
├── docs/
│   └── ... (specs, tech)
├── src/
│   ├── main.rs                  # Rust CLI entry (clap)
│   ├── cli/                     # CLI commands (auth, chat, run)
│   ├── agent/                   # Agent loop
│   ├── config/                  # Configuration TOML parsing
│   ├── context/                 # Context assembly (AGENTS.md, skills)
│   ├── integrations/            # External systems (MCP, Subprocess)
│   ├── llm/                     # LLM Clients (OpenAI, Anthropic, etc.)
│   ├── sandbox/                 # Hybrid sandbox (bwrap, boundaries)
│   ├── session/                 # Session management
│   ├── tools/                   # Tool registry and executor
│   └── tui/                     # ratatui interface
└── tests/
```

## Key Directories & Ownership

- `src/agent/` — The heart of the agent. Planning, execution strategies, and the LLM loop.
- `src/tools/` — Tool registration, classification, gates, and implementations.
- `src/sandbox/` — Direct bwrap execution and environment boundaries.
- `src/cli/` and `src/tui/` — Presentation layers. They should stay relatively thin.
- `src/integrations/` — All external system interactions.
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
- Full pure-Rust port of the agent loop → drop the Python subprocess bridge.
- A web dashboard → add a `fe/` directory.
- Background services → consider a `srv/` directory.

The current design (Rust CLI as the stable distribution vehicle + Python for complex agent logic) gives us fast iteration on the agent while providing users a single `nib` binary.

## References

- `docs/tech/architecture.md` — Base architecture.
- `docs/tech/ci.md` — Build, CI, release, and installation details.
- `docs/tech/backend_rust.md` — Rust core conventions.
- `docs/specs/done/ft_004_llm_integration_and_agent_loop.md` — LLM + agent loop spec.
- Central workspace references for patterns.

Update this document whenever the top-level layout changes significantly.
