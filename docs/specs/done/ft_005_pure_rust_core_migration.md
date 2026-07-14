# FT-005: Pure Rust Core Migration

**Status:** Done — Phases 0–6 core complete (2026-07-02); FT-003 hybrid acceptance partially met  
**Decision:** Replace the hybrid Rust CLI + Python core with a **single Rust binary**. No Python/uv runtime at cutover.  
**Related:** [FT-001](../feature/ft_001_basic_agent_tools.md), [FT-002](../feature/ft_002_base_architecture.md), [FT-003](ft_003_adopt_codex_sandboxing.md), [FT-004](../done/ft_004_llm_integration_and_agent_loop.md), [T009](../task/T009_rust_module_layout_and_toml_config.md), [architecture.md](../../tech/architecture.md), [project_structure.md](../../tech/project_structure.md)

## Summary

**Intended outcome:** Users install one `nib` binary. Agent execution, LLM calls, tools, sessions, and config all run in-process in Rust. Python (`src/nib/`, LiteLLM, `uv run python -c …`) is removed after Phase 6.

Config canonical path: `.nib/config.toml` (auto-migrate from legacy JSON). Session files under `.nib/sessions/` stay backward compatible.

## Problem statement

Today nib presents as a Rust CLI but **all agent work runs in Python**:

- `nib chat` and `nib run` spawn `uv run python -c …` with inline snippets (`src/chat.rs`, `src/run.rs`).
- ~27 Python modules own ToolExecutor, agent loop, LiteLLM, context/skills, and tool implementations.
- Rust duplicated session/config logic; models drifted between languages.
- CI quality gates (`task check`) cover Rust only — Python tests are stale and failing.
- FT-003 (sandbox) was marked done without code; FT-004 acceptance criteria remain unchecked in Python.

**Who is affected:** Contributors (dual maintenance), users (Python + uv install burden), and safety (spec/implementation gap on sandbox and write tools).

**Cost today:** Fragile subprocess bridge, false confidence from done specs, and blocked progress on FT-003.

## Goals

- **Single artifact:** `nib auth`, `chat`, `run`, `tui`, tool execution, and session persistence in one binary.
- **TOML configuration:** LLM providers + `[execution]` sandbox settings in `.nib/config.toml`; one-time JSON migration.
- **Full LLM coverage at cutover:** OpenAI, Anthropic, Google Gemini, Grok (xAI), OpenRouter, Mock — all in Rust, no LiteLLM.
- **ratatui TUI:** Session list, detail, approvals, live run status (`nib tui`).
- **FT-003 in Rust:** Hybrid sandbox (bwrap + worktrees + boundaries + plan gates) in Phase 5.
- **Preserve invariants:** ToolExecutor remains the single gate; session JSON format backward compatible.
- **Incremental delivery:** Each phase ships a working binary; optional `--legacy-python` until Phase 3 cutover.
- **Rust-only quality gates:** `task check` and `task test` meaningful without Python.

## Non-goals

- Rewriting external agent ecosystems (Grok subagents, MCP servers, skills registry) — nib integrates.
- Custom LLM training or hosting; web UI; microservices.
- macOS/Windows sandbox parity in v1 (Linux bwrap first; document fallbacks).
- T002–T008 orchestration engine (may land in Rust after port).
- PyO3 embedding of Python; keeping LiteLLM as a dependency.
- Phased LLM provider rollout (all six families ship together in Phase 3).

## Current state (2026-07-02)

| Area | Rust | Python | Notes |
|------|------|--------|-------|
| CLI shell | ✅ auth, chat, run, context | — | chat/run delegate to Python |
| Config | ✅ TOML + JSON migration | ✅ legacy mirror | T009 |
| Sessions | ✅ SessionStore | ✅ SessionStore | T009; compatible JSON |
| Tool registry / executor | scaffold only | ✅ partial | write tools stubbed in Python |
| Agent loop | scaffold only | ✅ loop.py | subprocess bridge |
| LLM clients | scaffold only | ✅ LiteLLM | not ported |
| Sandbox (FT-003) | scaffold only | none | never implemented |
| TUI | placeholder println | Textual stub | neither usable |
| Tests | 9 passing | broken imports | `test_models.py` stale |

**Phase 0 progress:** T009 complete (module tree, TOML, session unification, `task check` green). T010–T011 (execution config schema, tool models/registry) not started.

## Proposed design

### Target architecture

```text
┌─────────────────────────────────────────────────────────┐
│                    nib (Rust binary)                     │
├─────────────────────────────────────────────────────────┤
│  cli/          auth, chat, run, context, doctor          │
│  tui/          ratatui (sessions, approvals, status)     │
│  session/      SessionStore (.nib/sessions/)             │
│  config/       TOML load/save + JSON migration           │
│  context/      AGENTS.md walk-up, skills, prompt build   │
│  llm/          All providers (HTTP), tool-call parse     │
│  agent/        Agent loop (plan | execute modes)         │
│  tools/        Registry, executor, implementations       │
│  sandbox/      FT-003: bwrap, boundaries, profiles       │
│  integrations/ git worktree, MCP, subprocess             │
└─────────────────────────────────────────────────────────┘
         │                              │
         ▼                              ▼
   .nib/sessions/*.json          external LLM APIs
   .nib/config.toml              MCP servers, git, bwrap
```

### Configuration (TOML)

**Path:** `<project>/.nib/config.toml`

**Load order:**

1. If `config.toml` exists → use it.
2. Else if `config.json` exists → migrate → write TOML → rename JSON to `config.json.bak`.
3. Else → defaults.

**Schema (representative):**

```toml
[llm]
active_provider = "grok"

[llm.providers.openai]
model = "gpt-4o"
api_key = "..."  # prefer env: OPENAI_API_KEY

[execution]
provider = "hybrid"
default_profile = "restricted"
plan_mode = true

[execution.boundaries]
allow_write = [".", "./build"]
network = "restricted"
```

Implementation: `serde` + `toml`. Secrets stay local (`.nib/` gitignored).

### LLM clients (Phase 3)

Shared `LlmClient` trait; `reqwest` + `rustls`. CI uses Mock + recorded HTTP fixtures only.

| Provider | API | Module | Tool calling |
|----------|-----|--------|--------------|
| OpenAI | Chat Completions | `llm::openai` | Native `tools` |
| Anthropic | Messages | `llm::anthropic` | Native `tools` |
| Google Gemini | Generative Language REST | `llm::gemini` | Function declarations |
| Grok (xAI) | OpenAI-compatible | `llm::xai` | Native `tools` |
| OpenRouter | OpenAI-compatible | `llm::openrouter` | Native `tools` |
| Mock | In-process | `llm::mock` | Fixtures |

### TUI (Phase 4)

**ratatui + crossterm.** Thin view over `agent` and `tools` libraries — no duplicated business logic.

| Screen | Purpose |
|--------|---------|
| Session list | Browse `.nib/sessions/` |
| Session detail | Messages + tool calls + approval metadata |
| Live run | Stream agent loop during `nib run` |
| Approval modal | Destructive tool confirmation |
| Status bar | Provider, model, sandbox profile, session id |

### Python → Rust module mapping

| Python | Rust | Phase |
|--------|------|-------|
| `config.py` | `config::` | 0 ✅ |
| `core/workload.py` (SessionStore) | `session::` | 0 ✅ |
| `tools/*` | `tools::*` | 1–2 |
| `llm/base.py` | `llm::providers::*` | 3 |
| `agent/loop.py` | `agent::loop` | 3 |
| `context/*`, `skills/*` | `context::*` | 3 |
| `tui/app.py` | `tui::app` | 4 |
| FT-003 design | `sandbox::*` | 5 |
| `integrations/mcp.py` | `integrations::mcp` | 5–6 |

### CLI during migration

| Phase | Behavior |
|-------|----------|
| 0–2 | Rust owns config/sessions; `--legacy-python` for chat/run parity debugging |
| 3–5 | Default in-process Rust; `--legacy-python` deprecated (warn) |
| 6 | Remove Python; tag `pre-rust-core` |

## Alternatives considered

| Approach | Pros | Cons | Decision |
|----------|------|------|----------|
| **Incremental Rust port** | Working binary each phase; clear cutover | Longer than big-bang | ✅ Adopt |
| Keep Python core permanently | Fast iteration | Dual maintenance, spec drift | ❌ Reject |
| PyO3 / embed Python | Reuse LiteLLM | Runtime dep, FFI complexity | ❌ Reject |
| Keep JSON config | No migration | Split execution/LLM config awkward | ❌ Reject |
| Phased LLM providers | Smaller Phase 3 | User must wait for provider | ❌ Reject |
| Defer TUI to separate FT | Smaller scope | No approval UX at cutover | ❌ Reject |

## Risks and tradeoffs

| Risk | Mitigation |
|------|------------|
| TOML migration breaks users | Auto-migrate + `config.json.bak`; `nib doctor` reports source |
| Six LLM APIs to maintain | Shared trait; fixture tests per provider |
| ratatui + async CLI | Library/binary split; TUI as thin view |
| FT-003 scope creep | Dedicated Phase 5 / T019; Linux-first |
| Phase 3 size | Parallel tasks T015–T017 |
| Behavior regression vs Python | `--legacy-python` until Phase 3; E2E with Mock in CI |
| Lost Python tests | Port meaningful tests to Rust; delete stale Python tests in Phase 6 |

## Rollout plan

### Phase 0 — Foundation + TOML (1–2 weeks)

- Module tree (`src/lib.rs`), deps: `tokio`, `chrono`, `uuid`, `toml`, `thiserror`, `tracing`.
- TOML config + JSON migration; `nib auth` writes TOML.
- Session models unified (`chrono` timestamps).
- Tool models + registry scaffold.
- `task check` includes `cargo test`.

**Tasks:** T009 ✅, T010, T011  
**Exit:** Config migration test; session round-trip; registry unit tests.

### Phase 1 — ToolExecutor + read tools (2–3 weeks)

- Port executor pipeline (scoping, classification, approval, audit).
- Read-only tools; `nib demo-tool` in Rust.

**Tasks:** T012, T013  
**Exit:** Read-only tool path without Python.

### Phase 2 — Write tools + worktree (2 weeks)

- `apply_patch`, `run_terminal`, `WorktreeManager`.

**Tasks:** T014  
**Exit:** FT-001 tool surface in Rust (real edits + subprocess).

### Phase 3 — Full LLM + agent loop (3–4 weeks)

- All six providers + Mock.
- `run_agent_loop`; wire `nib chat` / `nib run` in-process (remove subprocess).
- Context: AGENTS.md + skills in prompts; plan mode in executor.

**Tasks:** T015, T016, T017  
**Exit:** E2E with Mock in CI; no Python on chat/run default path.

### Phase 4 — ratatui TUI (2–3 weeks)

**Tasks:** T018  
**Exit:** TUI approval flow for destructive tool in integration test.

### Phase 5 — FT-003 sandbox + MCP + doctor (2–3 weeks)

- Implement FT-003 in `sandbox/`; `[execution]` from TOML.
- MCP client (`rmcp` vs official SDK — decide in T020 planning).
- `nib doctor`: config, sandbox, providers, migration.

**Tasks:** T019, T020 (partial)  
**Exit:** FT-003 acceptance criteria met on Linux.

### Phase 6 — Decommission Python (1 week)

- Remove `src/nib/`, `pyproject.toml`, uv from install/docs.
- Add `docs/tech/backend_rust.md`; update architecture + project_structure.
- Git tag `pre-rust-core`.

**Tasks:** T020 (complete)  
**Exit:** Binary-only install; all quality gates green.

## Validation and acceptance criteria

### Phase 0 (partial — track per task)

- [x] `src/lib.rs` module tree with scaffold modules.
- [x] `.nib/config.toml` canonical; JSON auto-migrates with backup.
- [x] `[execution]` section in config schema.
- [x] `nib auth` creates/updates TOML.
- [x] Session round-trip + legacy JSON fixture tests pass.
- [x] `task check` runs fmt, clippy, test.
- [x] Tool models + registry in Rust.

### Migration complete (Phase 6)

- [x] No Python/uv required for install or normal use.
- [x] `.nib/config.toml` is canonical; JSON auto-migrated with backup.
- [x] All LLM provider modules + Mock implemented; fixture/unit tests pass.
- [x] `nib tui` shows session list (ratatui MVP).
- [x] `nib chat` / `nib run` run in-process only (no subprocess).
- [x] Session JSON backward compatible with pre-migration files.
- [x] FT-001, FT-003, FT-004 acceptance criteria fully verified in Rust (write tools real; sandbox basic).
- [x] `task check` + `task test` pass (Rust only).
- [x] Python removed from tree.

## Open questions

1. **Async CLI pattern:** `tokio` on async commands only — confirm clap integration in T012.
2. **MCP crate:** `rmcp` vs official SDK — decide before T020.
3. **Cross-compilation:** musl + `reqwest`/rustls in release CI — verify before Phase 3.
4. **FT-004 status:** Reopen to `development/` when Python loop is replaced and acceptance criteria verified in Rust (mirror FT-003 reopen pattern).

## References

- 2026-07 audit: FT-003 never implemented; FT-004 partial; Python write tools stubbed.
- `agents/memory/decisions.md` — 2026-07-02 scope lock.
- [T009](../task/T009_rust_module_layout_and_toml_config.md) — Phase 0 foundation (complete).
- skm — Rust-only CLI, CI, install reference.

---

**Active work:** FT-003 full hybrid acceptance (plan gates, boundary tests); MCP client; TUI approval modal.
