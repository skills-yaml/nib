# T009: Rust Module Layout + TOML Config Migration

**Status:** Done  
**Related feature:** [FT-005 Pure Rust Core Migration](../development/ft_005_pure_rust_core_migration.md) Phase 0  
**Depends on:** None (first migration task)

## Goal

Establish the Rust library/binary module tree, unify session models with proper timestamps, and replace `.nib/config.json` with **`.nib/config.toml`** including automatic one-time migration from legacy JSON.

## Scope

**In scope:**

- Add `src/lib.rs` and module directories (`config/`, `session/`, scaffold empty `tools/`, `llm/`, etc.).
- Move `session.rs` → `session/`; move config load/save → `config/` with TOML via `toml` crate.
- JSON → TOML migration on first load; backup as `config.json.bak`.
- Update `nib auth` wizard to read/write TOML.
- Add deps: `tokio`, `chrono`, `uuid`, `toml`, `thiserror`, `tracing` (reqwest deferred to T015).
- Fix session timestamps (replace hand-rolled ISO in `session.rs`).
- Extend `Taskfile.yml`: `task check` includes `cargo test`; document rust-only gates.
- Unit tests: config migration fixture, session round-trip, TOML round-trip.

**Out of scope:**

- ToolExecutor, LLM clients, TUI, sandbox (later tasks).
- Removing Python core.
- Changing session JSON format.

## Affected areas

- `src/lib.rs`, `src/main.rs`, `src/config.rs`, `src/session.rs` (refactored)
- `src/auth.rs` (TOML paths)
- `Cargo.toml`, `Taskfile.yml`
- `tests/` (Rust integration tests under `tests/` or `src/` modules)

## Success criteria

- [x] `cargo test` passes with config migration + session tests.
- [x] Existing `.nib/config.json` migrates to `.nib/config.toml` without data loss.
- [x] `nib auth` creates/updates TOML config.
- [x] Session files written by Rust remain loadable (fixture test).
- [x] `task check` runs fmt, clippy, test.
- [x] No behavior change to chat/run Python bridge yet (`--legacy-python` unchanged).

## Validation gates

```bash
task check
cargo test config_migration
cargo test session_roundtrip
```

## Risks

- Breaking auth wizard for users mid-migration — mitigate with backup file and doctor message (doctor full impl in T020; log migration in auth output for now).

## Exit

PR references T009 + FT-005 Phase 0; FT-005 Phase 0 exit criteria checked off.
