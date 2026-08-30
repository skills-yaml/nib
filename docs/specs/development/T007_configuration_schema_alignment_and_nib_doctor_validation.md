# T007: Configuration Schema Alignment + "nib doctor" Validation

**Status:** Development

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

> Historical proposal note: the YAML/Pydantic design below captures the pre-Rust
> baseline. The 2026-07-15 reconciliation defines project `.nib/config.toml`, strict
> Rust validation, and the shipped `nib doctor` contract.

## Historical Problem Statement (Proposal-Time)

nib lacks a unified, extensible configuration schema aligned with advanced agent runtimes (covering model, agent bounds, terminal, compression, memory, approvals, etc.). Current config is scattered (pyproject, placeholders). Additionally, there is no system introspection/validation tool equivalent to "doctor" that MUST pass with exit code 0, ensuring the runtime is healthy before execution. This leads to brittle setups and hard-to-debug issues in production-like agent deployments.

## Goals

- Define and implement a complete config schema (YAML/TOML) covering all engine aspects: model/provider/context_length, agent/max_turns/tool_enforcement, terminal/backend/timeout, compression, memory, approvals.mode, workload, mcp, skills.
- Add "nib doctor" (CLI command) that validates config, environment, permissions, skills/MCP connectivity, and runtime readiness (MUST exit 0 on success).
- Integrate with T003 (compression/memory), T005 (state machine), T006 (MCP/skills), T004 (daemons).
- Support profiles (T004) for per-workspace overrides.

## Non-Goals

- GUI config editor (CLI + file-based for v1).
- Remote config management.

## Historical Proposed Design

- Config in `~/.nib/config.yaml` (or project-local .nib/config.yaml), parsed with Pydantic.
- Schema sections as in T002.
- `nib doctor` command: Checks:
  - Config validity and required fields.
  - Git/worktree availability.
  - MCP server reachability.
  - Skills discoverable.
  - Workload DB writable.
  - Permission layers functional (test approvals).
- Exit 0 only if all pass; output diagnostics.

Update CLI to include `nib config edit`, `nib doctor`.

## Alternatives Considered

- Use only environment variables: Rejected — less structured for complex schemas like compression/memory.
- External config service: Overkill for local agent.

## Risks and Tradeoffs

- Config drift across profiles (mitigation: validation in doctor and runtime init).

## Rollout Plan

1. Define schema in code.
2. Implement doctor checks.
3. Wire into runtime startup (fail fast if invalid).
4. Tests and docs.

## Validation and Acceptance Criteria

- Full schema documented and parsed.
- `nib doctor` exits 0 on healthy setup, non-zero with clear errors otherwise.
- Runtime respects config (e.g., compression triggers per threshold).
- Aligned with T002 spec.

## Open Questions

- Default values and migration from current placeholders?

## Reopened Audit (2026-07-15)

Scope: complete and validate every runtime config section, apply defaults correctly,
wire runtime consumers, and make doctor fail with actionable diagnostics on invalid
or unavailable required capabilities.

Affected areas: `src/config/`, `src/doctor.rs`, runtime constructors, CLI docs, and
configuration/doctor tests.

Validation gates: valid/invalid/migration/runtime-consumption tests, doctor exit-code
smokes, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Use one strict project TOML schema with profile-aware runtime consumers, safe
migration/editing, credential redaction, and actionable doctor checks.

### Acceptance Criteria

- [x] `.nib/config.toml` covers providers, agent bounds, terminal, execution boundaries, approvals, compression, memory, workload, skills, MCP, daemons, and profiles.
- [x] Legacy JSON migrates with backup and corrupt configuration fails closed.
- [x] Config writes are locked/atomic and concurrent process updates are preserved.
- [x] `nib doctor` distinguishes healthy, invalid-config, credential, skill, daemon, permission, and MCP failures.
- [x] CLI config display redacts primary and rotating credentials by default.
- [x] Fresh local repository gates are green on the reconciled tree.
- [ ] Windows and macOS runtime gates are green on the reconciled tree.

### Affected Areas

`src/config/`, `src/config_cmd.rs`, `src/doctor.rs`, `src/auth.rs`, runtime
constructors, CLI docs, and config/doctor tests.

### Implementation Evidence

- `src/config/mod.rs` defines/validates the canonical schema and atomic migration/update paths.
- `src/doctor.rs` checks profiles, Git/worktrees, permission smoke, skills, daemons,
  credentials, and configured MCP server reachability.

### Validation Evidence

- `tests/config_migration.rs` and `tests/config_integrity.rs` cover migration,
  round-trip, and cross-process updates.
- `tests/doctor_cli.rs` covers healthy, invalid, and configured MCP initialization exits.
- `src/config_cmd.rs` covers redaction, validation, and editor failure behavior.
- The rebuilt release binary returned success from `nib doctor` in a fresh Git
  repository with an isolated home on 2026-07-15, using safe Mock/default
  configuration and reporting every runtime check healthy.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
remediation gates below are authoritative for completion.

- [x] Focused config, migration, concurrency, command, and doctor tests exist.
- [x] Final healthy `nib doctor` invocation.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

No remote configuration service or GUI editor is implemented, consistent with the
non-goals. The statement that no in-scope gap remained is superseded by the persistence
remediations below.

## Final Configuration Persistence Review Remediation (2026-07-15)

### Scope

Bind configuration serialization and reads to retained directory/file identities so a
replaced `.nib` directory, `config.toml`, or lock path cannot split writers or inject a
transient configuration. This boundary must preserve the exact credential/redaction
set consumed by detached workload workers.

### Acceptance Criteria

- [x] The config lock uses a no-follow open, opened/path identity checks, and a persistent
  anchor outside the replaceable `.nib` directory.
- [x] Config reads retain the opened handle, compare it with a no-follow path re-open,
  and reject symlinks plus every Windows reparse-point type before accepting bytes.
- [x] TOML migration, backup, edit rollback, and atomic save resolve relative to a
  retained `.nib` capability and fail closed on parent detachment or identity change.
- [x] Concurrent legitimate updates remain serialized and no failed read/write path
  replaces valid configuration with defaults or partial state.
- [x] Detached workers use the identity-validated configuration and cannot persist a
  credential revealed through a transient forged config with a reduced redaction set.
- [x] Deterministic child-process regressions replace the regular lock/config file and
  complete `.nib` directory during open/read/publish; all contenders block or fail
  closed, including Windows non-symlink reparse paths.

### Affected Areas

`src/config/mod.rs`, shared stable-directory/lock helpers, profile/runtime construction,
durable-worker redaction, config migration tests, and process-level integration tests.

### Validation Gates

Focused config identity, lock replacement, parent detachment, migration, and worker-
redaction tests; `task test`, `task check`, `task coverage`, Windows CI `task test`, and
isolated release-binary smoke.

### Implementation Evidence

- `src/config/mod.rs` opens `.nib` as a retained `StableDirectory`, reads through a
  no-follow handle with post-read identity checks, and performs migration, backup,
  rollback, and atomic save relative to that capability.
- `.nib/config.toml.lock` is bound to the persistent project-root anchor provided by
  `with_file_lock_in`; the pre-lock and protected `.nib` capabilities must have the
  same identity before a configuration operation starts.
- `src/daemons/workload.rs` verifies that a durable terminal worker rejects a transient
  forged configuration with fewer redaction credentials before execution or result
  publication, then persists only redacted output under the canonical configuration.

### Focused Validation Evidence

- `cargo check --lib`: passed.
- `cargo test --lib config::tests -- --test-threads=1`: 26 passed, including a
  forked reader held at a post-open barrier while its parent replaces the regular
  `config.toml` path.
- `cargo test --lib daemons::workload::tests::terminal_worker_rejects_transient_config_with_reduced_redaction_set -- --exact --test-threads=1 --nocapture`: 1 passed.
- `cargo test --test config_integrity -- --test-threads=1`: 3 passed, including
  cross-process serialization and persistent-lock replacement coverage.
- `cargo test --test config_migration -- --test-threads=1`: 2 passed.
- Windows-only config and filesystem regressions reject non-symlink directory junctions
  using the reparse-point attribute; execution remains a Windows CI gate.
- `cargo fmt -- --check`: passed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed.

## Final Conditional Configuration Commit Remediation (2026-07-15)

### Scope

Prevent an identity-validated configuration from being followed by an unconditional
overwrite of a newer regular file, recover bounded crash artifacts, and remove fail-open
runtime configuration and persistence diagnostics.

### Acceptance Criteria

- [x] Configuration save/update retains the opened prior `config.toml` or proves absence
  and verifies that exact expectation immediately before publication. The public
  snapshot save takes a mutable configuration, uses its legacy-compatible persisted
  revision as the CAS token, rejects stale or overflowing revisions, and writes the
  committed revision back into the caller. Legacy files without a revision load at zero.
- [x] A regular-file replacement between validated read and handle-bound no-replace
  publication is preserved, the operation fails closed, and no default or partial
  configuration is published.
- [x] Configuration temporary files use a deterministic bounded namespace. Under the
  configuration lock, recovery conditionally quarantines only identity-matching,
  unlocked pre-evacuation artifacts; ambiguous prior state is preserved and fails
  closed after process loss.
- [x] On Unix, cleanup conditionally detaches the exact opened artifact into quarantine,
  preserves ambiguous replacements, and reports unverified residual physical cleanup.
  Exact unlink after malicious same-UID pathname replacement is not claimed because
  that peer model is outside nib's isolation boundary.
- [x] Runtime and doctor callers use strict configuration/session results; compatibility
  fallback APIs are explicitly named and cannot hide corrupt or detached state.
- [x] Same-process and real child-process regressions replace `config.toml` at the commit
  barrier and kill a writer after temp fsync; restart preserves canonical configuration.
  `real_child_config_commit_barrier_and_fsync_crash_recovery` covers the concrete adapter
  and the shared atomic crash matrix proves ambiguous post-evacuation recovery fails
  closed without inventing canonical state.

### Affected Areas

`src/daemons/state.rs`, `src/config/mod.rs`, `src/doctor.rs`, `src/tui/mod.rs`,
configuration integrity tests, and detached-worker configuration consumers.

### Validation Gates

Focused conditional-commit, crash-recovery, strict-doctor, migration, and redaction
tests; `task test`, `task check`, `task coverage`, Windows CI `task test`, and isolated
release-binary smoke. Local aggregate validation, the locked build, and Linux
release/PTY smoke passed on 2026-07-16; Windows/macOS runtime criteria remain unchecked
until executed.

### Focused Validation Evidence

- `cargo test --lib config::tests::compatibility_loaders_ -- --test-threads=1`: 3
  passed. Missing configuration reports `ConfigSource::Default`; malformed TOML and
  simulated detached configuration state are propagated by both compatibility loaders.

## Remaining Implementation Plan

1. Execute Windows and macOS configuration, migration, doctor, identity, and recovery
   gates on their configured platforms and remediate platform-specific failures.
2. Rerun the canonical Task gates and two-stage review before moving T007 to `done/`.

## Current Risks

- Residual physical cleanup remains explicitly unverified when pathname ownership
  cannot be retained through unlink; hostile same-UID peers require an external
  isolation boundary.
- Windows and macOS configuration identity/recovery behavior remains unexecuted; fixes
  must preserve strict corruption propagation and fail-closed publication.

## Doctor-Guided OpenAI Transport Repair (2026-08-17)

[T027](../done/T027_doctor_guided_openai_transport_repair.md) extends runtime-readiness
diagnosis with an explicitly invoked `nib doctor --fix` repair for one narrowly
eligible canonical OpenAI transport configuration. The mutation re-evaluates current
state under the existing configuration lock, commits atomically, and performs no write
when no repair is needed, so idempotent runs do not advance the revision. Ordinary
doctor remains read-only, custom gateways remain excluded, and neither mode performs a
live provider capability probe.

The separate `nib doctor --fix --confirm-no-legacy-processes` surface is an explicit
operator attestation for T004/FT-015's one-time offline delegation-lock migration. It
requires all prior nib binaries to be stopped and disabled, persists an exact
capability-bound pending/completed epoch, and has no environment-only bypass. `--fix`
without that confirmation retains the OpenAI-only behavior above; ordinary doctor and
ordinary runtime operations never consume or delete legacy delegation state.
The confirmation does not authorize recursive repair of ambiguous native-origin
staging. Only a sole, valid receipt bound to that exact staging identity can resume;
unmarked, mismatched, or extra-content staging is preserved with actionable inspection
and exact-removal guidance.
