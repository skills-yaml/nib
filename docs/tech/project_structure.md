# Project Structure

nib CLI is implemented in **Rust** (primary binary, following skm structure) with a **Python core** for the agent loop, LLM clients, and tool execution (hybrid).

See skm's project_structure.md for reference on CI/install layout.

- Rust: `src/main.rs`, `src/auth.rs`, `src/chat.rs`, `src/run.rs`, `src/config.rs`, `src/session.rs`, `src/updater.rs`, etc.
- Python core: `src/nib/` (agent/loop.py, llm/base.py, core/workload.py (sessions), tools/, etc.)

The `nib` command is the compiled Rust binary. Python is invoked via subprocess for agent steps during the transition.

## High-Level Layout

```
nib/
├── Cargo.toml
├── Cargo.lock
├── build.rs
├── pyproject.toml
├── uv.lock
├── Taskfile.yml                 # All dev, check, test, and build commands (cargo + py)
├── AGENTS.md
├── README.md
├── .gitignore
├── .github/workflows/           # ci.yml + release.yml (skm style)
├── scripts/                     # install.sh, install.ps1, first-time-setup.sh
├── docs/
│   └── ... (specs, tech)
├── src/
│   ├── main.rs                  # Rust CLI entry (clap)
│   ├── auth.rs, chat.rs, run.rs, ...
│   ├── config.rs, session.rs, updater.rs, version.rs
│   └── nib/                     # Python core (hybrid)
│       ├── agent/loop.py
│       ├── llm/base.py (LiteLLM + Mock)
│       ├── core/workload.py (sessions in .nib/)
│       ├── tools/
│       └── cli/app.py (legacy Typer reference)
└── tests/
```

## Key Directories & Ownership

- `src/nib/core/` — The heart of the agent. Workload model, planning, execution strategies, verification/reconciliation. This should be the most testable and framework-light part of the system.
- `src/nib/cli/` and `src/nib/tui/` — Presentation layers. They should stay relatively thin and delegate to `core/`.
- `src/nib/integrations/` — All external system interactions (spawning other agents, git operations, MCP/CLI bridges). Isolate these for easier testing and swapping.
- `docs/specs/` — Product truth. Never implement major behavior without a corresponding spec or task plan.
- `docs/tech/` — Engineering conventions. Keep them up to date as the project evolves.

## Package Rules

- Rust binary (`nib`) is the user-facing CLI (built from `src/` with `cargo`).
- Python lives under `src/nib/` and is invoked by the Rust CLI (via `uv run python` or `python -m`) for the agent loop and LLM/tool logic during the hybrid phase.
- Use the `src/` layout for Python.
- All Python imports are absolute from the `nib` package: `from nib.core.workload import ...`
- Python parts can still be run directly with `uv run python -m nib ...` for debugging.

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
- `docs/tech/backend_python.md` — Python core conventions.
- `docs/specs/done/ft_004_llm_integration_and_agent_loop.md` — LLM + agent loop spec.
- Central workspace references for patterns.

Update this document whenever the top-level layout changes significantly.
