# T003: Implement Context Engine with Dynamic Compression and Session Management

**Status:** Development

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

> Historical proposal note: the problem and Python/SQLite design sections capture the
> pre-Rust baseline. The 2026-07-15 reconciliation defines the shipped bounded Rust
> context and profile-session implementation.

## Historical Problem Statement (Proposal-Time)

Current nib context handling (in `context/` loaders) assembles AGENTS.md, skills, project standards, and workload snapshots statically. This leads to bloated contexts with raw tool outputs, terminal dumps, and unprocessed history. As sessions grow, this causes high token usage, degraded reasoning quality, and loss of long-term session state across restarts. Without dynamic compression and proper session modeling (as indexed alternating message sequences), the agent cannot efficiently retain crucial facts while reclaiming context space. This mirrors common failure modes in autonomous agents where context bloat reduces accuracy and increases costs.

## Goals

- Implement a Context Engine that dynamically compresses context once thresholds are reached (e.g., 50% of limit), synthesizing historic facts and progress while preserving system elements, AGENTS.md rules, active skills, and key workload state.
- Model sessions as durable, indexed sequences of alternating user/assistant/tool messages with strict role alternation invariants.
- Support cross-session persistence of session histories and discrete factual memory (environment configs vs. user identity).
- Integrate seamlessly with existing context loaders, ToolExecutor, WorkloadStore, and permissions model.
- Enable the runtime loop to maintain efficiency over long-running tasks and multi-turn interactions.
- Provide hooks for the ASCII sequence diagram in T002 to reflect compression triggers and session appends.

## Non-Goals

- Full replacement of the current static context assembly (enhance and extend it).
- Inventing new LLM architectures for compression (use standard auxiliary LLM calls or simple heuristics initially).
- Handling non-text modalities (focus on text-based sessions and tool outputs).
- Real-time streaming compression (batch at turn boundaries).

## Historical Proposed Design

Extend the existing `src/nib/context/` module and integrate with `core/` runtime components.

**Core Components:**
- **Context Builder**: Enhances current `assemble_context()` to track token usage against model `context_length` (from config).
- **Compression Trigger**: When threshold hit (configurable, default 0.50):
  - Preserve: System prompt, AGENTS.md content, active skills instructions, current task state.
  - Compress: Historic conversation logs and raw tool results by sending to an auxiliary LLM with instructions to "summarize historic facts, code progress, decisions, and lessons learned into a compact narrative".
  - Replace: Old logs with the synthesized summary + metadata (original turn range, key entities).
  - Target: Reduce to ~20% of original size for the compressed segment.
- **Session Manager**: Store sessions as indexed lists (e.g., in SQLite or JSONL alongside WorkloadStore). Enforce:
  - Strict alternation: User → Assistant (tool call or text) → Tool (if any) → Assistant (resolution).
  - No consecutive same-role messages (squash or error on violation).
- **Memory Store**: Add a discrete layer (separate from main workload SQLite):
  - Environment memory: Key-value for facts, preferences, learned behaviors (JSON-backed or SQLite table).
  - User memory: Identity records, long-term profile.
  - Persist across sessions/profiles.
- **Integration Points**:
  - Call from runtime state machine (see T005).
  - Feed compressed context into ToolExecutor for tool decisions.
  - Update WorkloadStore with session snapshots and compression events for audit.
  - Respect permissions: Compression decisions logged as tool-like events if they affect execution.
- **Config** (align with T007): Add to schema:
  ```yaml
  compression:
    enabled: true
    threshold: 0.50
    target_ratio: 0.20
  memory:
    enabled: true
    provider: "built-in"  # or "sqlite"
  ```

**Implementation Approach:**
- New `src/nib/context/compression.py` for the trigger and LLM summarizer.
- Enhance `src/nib/context/loader.py` to produce compressed views.
- Add session models in `core/models.py` (e.g., `Session` with indexed messages).
- Use existing aiosqlite for persistence; add tables for sessions and memory KV.
- Expose via new methods in Context Loader: `build_compressed_context()`, `append_to_session()`.
- For the sequence diagram (in T002): Insert steps for "13. [Optional] Context Compression Trigger" and "12. Append to chat context".

This design reuses nib's existing workload and permission infrastructure while adding the missing compression and session durability from the target architecture.

## Alternatives Considered

- Simple truncation or sliding window: Rejected — loses too much factual history and violates "retaining crucial session context".
- Always use full history with external RAG: Rejected for v1 — adds complexity; prefer in-context compression first for low-latency local agent.
- External vector DB for memory: Rejected initially (use simple KV for now; can evolve in rollout).
- No strict role alternation enforcement: Rejected — core invariant from the spec to prevent malformed prompts.

## Risks and Tradeoffs

- **Compression Fidelity Risk**: Summaries may drop subtle details (mitigation: preserve key entities/links in metadata; make summarizer prompt configurable; test on real coding sessions).
- **Performance Tradeoff**: Auxiliary LLM call for compression adds latency and cost (tradeoff acceptable for long sessions; optional and threshold-based).
- **Storage Growth**: Session histories + memory KV can bloat (mitigation: tie cleanup to maintenance daemons in T004; retention policies).
- **Integration Complexity**: Must not break existing ToolExecutor or permissions flows (design keeps compression as a pre-execution step in the loader).

## Rollout Plan

1. **Phase 1 (Foundation)**: Implement basic compression trigger and session append in context module. Add models and persistence. Wire into current demo-tool for testing.
2. **Phase 2 (Integration)**: Connect to runtime state machine (T005). Update ToolExecutor to use compressed context. Add logging of compression events to workload.
3. **Phase 3 (Persistence & Memory)**: Separate Memory Store. Support cross-session load/save.
4. **Phase 4 (Polish)**: Config integration (T007), tests (T008), and diagram validation. Update architecture.md and FT-002.
5. Use subagent-driven-development (from existing skills) for implementation steps. Validate against the sequence diagram in T002.

## Validation and Acceptance Criteria

- Context compresses when threshold reached, reducing size while retaining system/AGENTS/skills facts (measured via token count before/after).
- Sessions stored as indexed alternating sequences; role invariant enforced (no consecutive same roles).
- Memory Store persists discrete env/user data across restarts and sessions.
- Compression and append steps appear correctly in the end-to-end sequence diagram execution.
- All paths respect permissions (e.g., compression decisions audited).
- `task test` and `task check` pass; deterministic compression runtime and E2E tests
  demonstrate bounded long-interaction context. The proposal-time `demo-tool` route is
  superseded by the in-process Rust agent loop.
- Matches symphony structure: clear problem, goals, design, risks, etc.

## Open Questions

- Exact auxiliary LLM prompt for summarization (tunable via skills?).
- How to handle very large tool outputs (e.g., pre-compress before session append)?
- Integration with existing workload "backlog/working/done" buckets (should compression affect task status?).
- Performance benchmarks for compression in real coding workloads.

## Reopened Audit (2026-07-15)

Scope: enforce indexed role-safe sessions, honor compression target ratios, preserve
raw audit history, record compression events, and make env/user memory operational.

Affected areas: `src/context/`, `src/session/`, `src/agent/`, and long-session tests.

Validation gates: role/error tests, memory restart tests, measured before/after
compression tests, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Bound aggregate model inputs, preserve indexed raw session history, summarize old
history at configured thresholds, and inject profile memory plus workload state.

### Acceptance Criteria

- [x] Runtime and planning payloads count system messages, history, and tool schemas against `context_length`.
- [x] Compression records before/after measurements while preserving the raw transcript.
- [x] Session roles and indices are validated and corrupt state fails closed.
- [x] Environment and user memory persist separately and are included in bounded context.
- [x] Long AGENTS content is complete when budget permits and head/tail marked only under aggregate pressure.
- [x] Fresh local repository gates are green on the reconciled tree.
- [ ] Windows and macOS runtime gates are green on the reconciled tree.

### Affected Areas

`src/context/budget.rs`, `src/context/compression.rs`, `src/context/mod.rs`,
`src/session/`, `src/agent/`, and long-session tests.

### Implementation Evidence

- `src/context/budget.rs` owns aggregate prompt/tool/history budgets.
- `src/context/compression.rs` and `src/session/mod.rs` persist summaries without
  deleting messages; `src/session/memory.rs` owns discrete profile memory.

### Validation Evidence

- `tests/compression_runtime.rs`:
  `compression_bounds_hot_context_and_retains_raw_audit_history`.
- `tests/session_roundtrip.rs`: corrupt-state, role/index, concurrency, restart, and
  path-escape tests.
- `src/context/budget.rs`:
  `aggregate_runtime_payload_is_bounded_and_preserves_critical_edges` and
  `long_agents_tail_is_complete_when_possible_and_marked_under_pressure`.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
remediation gates below are authoritative for completion.

- [x] Deterministic long-session, raw-audit, role, and aggregate-budget tests exist.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

Real-provider summary quality is not benchmarked; correctness uses deterministic mock
coverage. SQLite/JSONL is superseded by atomically written, locked profile JSON. This
earlier assessment is superseded by the persistence and deletion remediations below.

## Final Session Persistence Review Remediation (2026-07-15)

### Scope

Bind session locking, reads, atomic publication, deletion, and bounded enumeration to
retained directory and file identities. A replaced session file, lock path, or complete
sessions directory must not split serialization, redirect audit evidence, or let a
workload transition claim delivery that was published to an unowned path.

### Acceptance Criteria

- [x] Session and skill-usage locks use no-follow opens, opened/path identity checks,
  and persistent anchors outside the replaceable sessions directory.
- [x] Session lock artifacts use a fixed bounded stripe namespace rather than one
  lifetime artifact per requested session ID.
- [x] Session reads retain the opened handle, compare it with a no-follow path re-open,
  and reject Windows reparse points as well as symbolic links.
- [x] Session writes, deletes, and bounded enumeration resolve relative to a retained
  sessions-directory capability and fail closed when its visible path is detached.
- [x] Workload terminal/schedule and MCP cancellation delivery cannot commit success
  after session/audit publication failed or landed in a detached directory.
- [x] Deterministic process-level regressions replace ordinary session/lock files and
  the complete sessions directory before open, during read, and before/after publish;
  contenders block or fail closed and no replacement state is accepted.

### Affected Areas

`src/session/mod.rs`, `src/daemons/state.rs`, session delivery/reconciliation callers,
curator skill-usage locking, and session/durable-task integration tests.

### Validation Gates

Focused identity, lock replacement, directory detachment, paired-delivery, and Windows
reparse tests; `task test`, `task check`, `task coverage`, Windows CI `task test`, and
isolated release-binary smoke.

### Implementation Evidence

- `src/session/mod.rs` retains `StableDirectory` handles for the sessions directory
  and its parent, binds them with a hard-linked identity marker, and uses 64 stable
  session lock stripes plus one anchored skill-usage lock.
- Session loads retain the opened file through parsing and path-identity verification.
  Atomic writes verify the expected prior identity immediately before publication and
  verify the published content; delete and bounded list/aggregation paths use the same
  retained directory capability.
- `src/daemons/curator.rs` performs session cleanup enumeration and skill-usage
  aggregation through strict `SessionStore` result APIs, with the full aggregation and
  deletion decision kept inside the anchored skill-usage lock closure.
- Existing terminal and schedule publication paths convert session delivery errors to
  failed durable outcomes; cancellation remains non-success and records publication
  failures. MCP cancellation uses the same strict session result path.

### Replacement Regression Evidence

- Same process (`src/session/mod.rs`):
  `session_file_replacement_during_read_is_rejected`,
  `session_file_replacement_during_update_is_not_overwritten`,
  `session_lock_replacement_while_held_is_rejected`,
  `skill_usage_lock_replacement_while_held_is_rejected`,
  `whole_session_directory_replacement_is_rejected`, and
  `session_lock_artifacts_are_bounded_by_fixed_stripes`.
- Real child process (`tests/session_roundtrip.rs`):
  `child_process_rejects_session_file_replacement_during_update`,
  `child_process_rejects_replacement_lock_as_a_contender`, and
  `child_process_rejects_whole_session_directory_replacement`.
- Existing process coverage remains in
  `child_process_updates_do_not_lose_session_events_or_memory` and
  `killed_child_releases_session_lock`.

### Local Validation Evidence

- [x] `cargo check`.
- [x] `cargo test session::tests --lib -- --test-threads=1`.
- [x] `cargo test --test session_roundtrip -- --test-threads=1`.
- [x] `cargo test daemons::curator::tests --lib -- --test-threads=1`.
- [x] `cargo fmt -- --check` and focused `git diff --check`.
- [x] Strict `cargo fmt --all -- --check`, `cargo check --all-targets`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and `git diff --check`.
- [x] Fresh `task check`, independent `task test`, `task docs:check`, `task coverage`,
  locked release build, and Linux release/PTY smoke on 2026-07-16. The suite executes
  772 tests and coverage is 83.94 percent (53,734/64,015).
- [ ] Windows runtime execution remains a final platform gate. A Windows runtime and
  MSVC toolchain are unavailable on this host, and local Linux does not prove Windows
  reparse behavior.

## Final Conditional-Commit And Deletion Remediation (2026-07-15)

### Scope

Make every session mutation conditional on the exact file state that was read, make
destructive namespace detachment conditional on identity, bound crash-recovery
artifacts, and prevent generic session APIs from bypassing the authoritative
skill-usage lock. The supported Unix contract is exact namespace detachment; isolation
from a malicious peer already running as the same UID is outside the product boundary.

### Acceptance Criteria

- [x] Existing-session writes carry the opened prior file through the mutation and
  verify its identity immediately before publication; creation proves absence at the
  same point. Public snapshot saves take a mutable snapshot, use its persisted revision
  as the CAS token, reject stale or overflowing revisions, and write the committed
  revision back into the caller. Legacy files without a revision load at revision zero.
- [x] Conditional session deletion moves the visible entry to an unowned quarantine
  name without replacement, verifies the moved identity, and restores or fails closed
  if another file was substituted before that namespace transition.
- [x] On Unix, deletion conditionally detaches the exact opened inode into quarantine,
  preserves ambiguous replacements, and reports unverified residual physical cleanup.
  Exact physical unlink after malicious same-UID pathname replacement is not claimed;
  that peer model requires an external account, VM/container, or privileged broker.
- [x] Every API capable of changing `active_skills` or `skill_usage` participates in the
  shared skill-usage lock without recursive lock acquisition.
- [x] Session temporary publication uses a deterministic bounded namespace and
  conditionally quarantines only identity-matching, unlocked pre-evacuation crash
  artifacts under the owning lock. An
  unjournaled prior artifact, including target-missing plus prior-present, is preserved
  and causes recovery to fail closed.
- [x] Runtime session enumeration surfaces corruption, detachment, and scan-limit errors
  instead of converting them to an empty session list.
- [x] Same-process and real child-process barriers cover replacement immediately before
  commit/delete, recent skill usage during cleanup, and process loss after temp fsync.
  `real_child_session_commit_barrier_and_fsync_crash_recovery` exercises the concrete
  session adapter and `real_child_atomic_fsync_crash_recovery_matrix` covers recoverable
  pre-evacuation and ambiguous post-evacuation crash states.

### Affected Areas

`src/daemons/state.rs`, `src/session/mod.rs`, session callers in `src/doctor.rs` and
`src/tui/mod.rs`, `src/daemons/curator.rs`, and session persistence tests.

### Validation Gates

Focused conditional-commit, quarantine, skill-lock, crash-recovery, and strict-caller
tests; `task test`, `task check`, `task coverage`, Windows CI `task test`, and isolated
release-binary smoke. Local aggregate validation and Linux smoke passed on 2026-07-16;
Windows/macOS runtime criteria remain unchecked until executed.

## Exact Audit Float Readback Remediation (2026-07-16)

### Scope

Keep the session store's post-publication structural verification exact for finite audit
timings and nested JSON numbers. Exact published bytes must not be rejected because the
JSON parser reconstructs a neighboring IEEE-754 value.

### Acceptance Criteria

- [x] `serde_json` uses its `float_roundtrip` parser for every session and audit read.
- [x] A deterministic regression persists `1.5519787360000001` in both
  `ToolCallRecord.duration_seconds` and nested JSON and verifies identical float bits.
- [x] Session readback errors identify the mismatched field without exposing stored
  session contents.
- [x] The concurrent delegation merge audit reproducer passes 20 consecutive runs.

### Affected Areas

`Cargo.toml`, `src/session/mod.rs`, session audit callers, and delegation integration
validation.

### Validation Gates

The exact float regression, concurrent merge stress, `task check`, `task test`,
`task coverage`, and strict format/Clippy/diff gates.

## Hosted Windows Enumeration Serialization Follow-up (2026-07-21)

### Scope

Serialize public strict session enumeration with the same anchored skill-usage lock that
guards existing-session writes and deletion. This prevents an in-progress atomic rewrite's
intentional target-evacuation window from appearing as a corrupt missing file while still
surfacing real recovery, identity, corruption, detachment, and scan-limit failures. Keep
the curator's internal enumeration entry point non-recursive because its aggregate path
already owns that lock.

### Acceptance Criteria

- [x] `SessionStore::list_result` holds the anchored skill-usage lock across recovery,
  bounded enumeration, and strict content validation.
- [x] A configured lock timeout is one absolute deadline shared by the outer mutation
  lock and every nested session validation lock.
- [x] `list_for_skill_usage` remains available to the curator while it already owns the
  mutation lock; no recursive lock acquisition is introduced.
- [x] A deterministic regression proves strict enumeration times out while the mutation
  lock is held and succeeds after release; existing corruption regressions remain strict.
- [ ] The exact PR revision passes the Windows MCP stdout-backpressure regression and the
  full hosted matrix.

### Affected Areas

`src/session/mod.rs`, MCP audit polling integration coverage, curator aggregation, T003
validation evidence, and the native CI matrix.

### Validation Gates

Focused session lock/enumeration tests, the Windows MCP backpressure regression,
`task test`, `task check`, `task docs:check`, Windows-target `task check:all-targets`,
`git diff --check`, and the exact-revision hosted CI matrix.

### Local Validation Evidence

The strict enumeration regression and existing corrupt-session regressions passed in
`task test` and `task check`. The revision also passed `task fix`, `task docs:check`,
`task check:all-targets TARGET=x86_64-pc-windows-msvc`, and `git diff --check`. Native
Windows runtime behavior and the exact hosted matrix remain open.

## Hosted Windows Directory Pinning Follow-up (2026-07-21)

### Scope

Align the real-child whole-session-directory replacement regression with the platform
contract already exercised by the same-process test. Unix must detach and replace the
directory while the child owns the session lock, then prove the child fails closed and
replacement state is not accepted. Windows must prove the live cross-process session lock
prevents the directory rename at the operating-system boundary, release and join the
child, and verify that its update commits to the unchanged original directory. Do not
weaken retained directory identity, lock, publication, or detachment validation.

### Acceptance Criteria

- [x] The real-child regression preserves the Unix detachment and fail-closed assertions.
- [x] On Windows, the same regression requires the live sessions-directory rename to fail,
  always releases and joins the child, and verifies the original child update succeeds.
- [x] File replacement and lock replacement regressions remain unchanged.
- [x] Canonical local, documentation, and Windows-target validation gates pass.
- [ ] The exact hosted Windows revision passes `session_roundtrip` and the full CI matrix.

### Affected Areas

`tests/session_roundtrip.rs`, T003 validation evidence, and the native CI matrix.

### Validation Gates

The session roundtrip integration suite, `task fix`, `task test`, `task check`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`,
`git diff --check`, fresh spec-compliance and code-quality reviews, and exact-revision
hosted CI.

### Local Validation Evidence

The platform-specific real-child regression passed in both `task test` and the complete
test rerun inside `task check`; its Unix branch retained successful namespace detachment,
child failure, and replacement preservation. The revision also passed `task fix`,
`task docs:check`, `task check:all-targets TARGET=x86_64-pc-windows-msvc`, and
`git diff --check`. The Windows target compiled the child-only pinning proof, unconditional
child release and join, original-directory assertions, and successful-update readback.
Native Windows execution and the exact hosted matrix remain open.

## FT-019 Explicit Compaction Reconciliation (T032, 2026-08-26)

T032 reuses this task's bounded summary persistence for an explicit `/compact`
maintenance run. Explicit invocation bypasses only the automatic threshold: the
enabled switch and ratio validation remain authoritative, raw messages remain
immutable, and summary publication now compares the complete raw message snapshot so
a concurrent append rejects the stale summary. The command creates no synthetic chat
message and records exact run plus compression evidence. This does not close T003's
native platform evidence requirements.

## Remaining Implementation Plan

1. Execute the Windows and macOS session/context runtime gates on their configured
   platforms and remediate any identity or persistence differences.
2. Rerun the canonical Task gates and two-stage review before moving T003 to `done/`.

## Current Risks

- Residual physical cleanup is deliberately reported as unverified when exact
  pathname ownership cannot be retained through unlink; hostile same-UID peers remain
  outside nib's isolation boundary.
- Windows and macOS session persistence and deletion behavior have not been executed on
  this Linux host.
