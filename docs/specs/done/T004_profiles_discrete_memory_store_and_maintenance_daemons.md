# T004: Profiles, Discrete Memory Store, and Maintenance Daemons (Cron/Curator)

**Status:** Done

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

> Historical proposal note: the problem and Python/SQLite design sections capture the
> pre-profile baseline. The 2026-07-15 reconciliation defines the shipped Rust,
> profile-scoped JSON implementation.

## Historical Problem Statement (Proposal-Time)

nib currently uses a monolithic WorkloadStore (SQLite for projects/tasks/tool history) without separation of concerns for different runtime workspaces. This leads to:
- Fragile state: No per-workspace "Profiles" that encapsulate custom env, skills, and context DBs, making it hard to switch contexts or isolate user data.
- Loss of cross-session memory: Behaviors, preferences, and learned facts are not persisted in discrete stores, causing repetitive steering.
- No automated maintenance: Old sessions, memory bloat, and stale data accumulate without Cron-like recurring jobs or Curator-style cleanup.

This directly conflicts with the need for durable, isolated, self-improving agent runtimes that persist lessons across sessions without manual intervention.

## Goals

- Introduce Profile concept: Per-workspace runtime isolation with custom .env, skills, and localized stores.
- Discrete Memory Store: Separate environment configurations (memory) from user identity records (user) in a durable (SQLite/JSON) format.
- Maintenance Daemons:
  - Cron: For offline recurring jobs (e.g., scheduled task reviews, background compression).
  - Curator: For cleaning old memory, sessions, and skills (with pinning for important items).
- Cross-session persistence of facts, preferences, and progress.
- Integration with ToolExecutor, permissions, context engine (T003), and workload buckets (backlog/working/done).
- Support for the runtime state machine (T005) and config (T007).

## Non-Goals

- Full distributed or cloud-based profiles (keep local-first).
- Complex multi-user tenancy (focus on single-user per profile).
- Real-time daemon execution in the main loop (offline/background where possible).

## Historical Proposed Design

Extend `core/` and add new modules for profiles/memory/daemons.

**Core Additions:**
- **Profile**: Dataclass/model identifying a workspace (id, root_path, custom_env, active_skills, memory_db_path, context_db_path). Loaded at startup or task activation. Maps to current project model but adds isolation.
- **Memory Store**: Discrete key-value (env memory for facts/behaviors; user memory for identity/preferences). Backed by SQLite (new tables or separate DB) or JSON for portability. Persist facts like "preferred test command", learned patterns, user style.
- **Maintenance Daemons**:
  - Cron: Scheduler (extend existing task runner or use Python `schedule`/`apscheduler`) for recurring tasks, e.g., "every 24h: compress old sessions", "on idle: review backlog".
  - Curator: Background process to archive/delete old data (sessions > N days, uncompressed history), with "pinned" exceptions for important skills/memory. Telemetry for usage.
- **Integration**:
  - Context Engine (T003) loads profile-specific memory and sessions.
  - ToolExecutor records against current profile.
  - WorkloadStore extended with profile_id foreign keys.
  - Daemons update workload (e.g., move stale tasks) and respect permissions (no destructive cleanup without policy).
- **Persistence**: Add to WorkloadStore or new `memory.py`:
  - Tables: profiles, memory_env, memory_user, sessions (indexed messages), daemon_logs.
  - Cross-session: Load profile on `nib init` or task start.

**Config Extension** (align with T007):
```yaml
profiles:
  default: "my-project"
  active:
    - id: "my-project"
      root: "/path/to/project"
      env: ".env.nib"
memory:
  enabled: true
  provider: "sqlite"
daemons:
  cron_enabled: true
  curator_enabled: true
  retention_days: 30
```

## Alternatives Considered

- Single monolithic store (current state): Rejected — leads to the fragile state problem.
- Full ORM like SQLAlchemy: Rejected for v1 (keep aiosqlite simple; evolve later).
- External services (e.g., vector DB for memory): Rejected (local-first; use built-in for now).
- In-memory only daemons: Rejected (need durability for cross-session).

## Risks and Tradeoffs

- **Complexity Risk**: Adding profiles/memory/daemons increases surface (mitigation: modular, start with defaults, extensive tests in T008).
- **Performance Tradeoff**: Daemons add background overhead (tradeoff for long-term usability; make configurable and low-priority).
- **Migration**: Existing users' data needs migration to profiles (plan: one-time script in rollout).
- **Isolation vs. Sharing**: Strong profiles may hinder cross-project learning (mitigation: optional "global" profile or shared memory subsets).

## Rollout Plan

1. **Phase 1**: Define Profile and Memory Store models/persistence. Basic load/save.
2. **Phase 2**: Implement Cron (recurring compression/review) and Curator (cleanup with pinning).
3. **Phase 3**: Integrate with T003 (context uses profile memory/sessions), T005 (runtime uses profile), ToolExecutor (records per profile).
4. **Phase 4**: Config support (T007), tests (T008), update architecture.md and docs. Provide migration for T001-era data.
5. Use existing skills (e.g., symphony for planning daemon jobs).

## Validation and Acceptance Criteria

- Profiles load custom env/skills per workspace; isolation verified (no cross-profile leakage).
- Memory Store persists facts across sessions/restarts (env vs. user separation).
- Daemons run (Cron jobs execute; Curator cleans old data, respects pins).
- Integration with workload buckets and permissions (daemons log as tool executions).
- Matches symphony structure and sequence diagram (T002) for persistence steps.
- `task test` passes; "nib doctor" (T007) validates daemons.

## Open Questions

- Retention policies and pinning UI (TUI/CLI commands?).
- How daemons interact with active TUI sessions (pause on user activity?).
- Exact schema for memory KV (simple strings or structured with timestamps?).
- Performance impact of frequent curator runs on large histories.

## Reopened Audit (2026-07-15)

Scope: implement profile isolation, persistent env/user memory APIs, scheduled jobs,
pin-aware curation, daemon audit records, migration, and doctor validation.

Affected areas: `src/profile/`, `src/session/`, `src/daemons/`, `src/config/`,
`src/doctor.rs`, and deterministic profile/daemon tests.

Validation gates: isolation/restart/pinning/scheduling tests, `nib doctor`,
`task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Resolve workspace profiles, isolate sessions/memory/skills/daemon state, persist
discrete environment and user memory, and run auditable cron/curator maintenance plus
detached durable workers.

### Acceptance Criteria

- [x] Profile roots, state paths, environment, skills, sessions, memory, and daemon state are isolated and bounded.
- [x] Legacy project session/memory state migrates once without deleting the source.
- [x] Cron cadence and curator cleanup/pins survive restart and fail closed on corrupt state.
- [x] Destructive cleanup requires explicit policy and is mirrored into session audit.
- [x] Durable terminal and schedule workers survive the invoking process and reconcile stale leases.
- [x] Fresh local repository gates are green on the reconciled tree.
- [x] Windows and macOS runtime gates are green on the reconciled tree.

### Affected Areas

`src/profile/`, `src/session/memory.rs`, `src/daemons/`, `src/config/`,
`src/doctor.rs`, and profile/durable-task tests.

### Implementation Evidence

- `src/profile/mod.rs` and `src/profile/migration.rs` own resolution and migration.
- `src/daemons/cron.rs`, `src/daemons/curator.rs`, and
  `src/daemons/workload.rs` own persisted maintenance and detached task state.

### Validation Evidence

- `src/profile/mod.rs`: `profiles_isolate_environment_skills_and_memory` and path/symlink guards.
- `src/daemons/curator.rs`: cleanup, pinning, corruption, concurrency, and symlink tests.
- `src/daemons/cron.rs`: cadence, restart, concurrent claim, and corrupt-state tests.
- `tests/durable_tasks.rs`: four cross-process terminal/schedule/redaction tests.
- The rebuilt release binary returned success from `nib doctor` in a fresh Git
  repository with an isolated home on 2026-07-15; it reported healthy profile,
  persistence, permission, daemon, and sandbox checks with zero existing sessions.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
remediation gates below are authoritative for completion.

- [x] Focused isolation, migration, restart, cleanup, pinning, and worker tests exist.
- [x] Healthy `nib doctor` result in the final gate run.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

Maintenance is activation/cadence driven rather than a permanently resident service.
That is the shipped local-first model; a resident daemon would require a separate spec.
This earlier assessment is superseded by the persistence remediations below.

## Final Quality Review Remediation (2026-07-15)

### Scope

Make durable task admission transactional and bounded, evict terminal in-memory
registrations, and ensure Unix workers are isolated from the caller's terminal process
group while a reaper owns every spawned child.

### Acceptance Criteria

- [x] Persisted durable tasks are admitted under a store-wide count bound without
  leaving orphan `prepared` records when registration fails.
- [x] Durable enumeration has an aggregate byte budget, while reconciliation processes
  large valid record sets without materializing every result at once.
- [x] Duplicate IDs across different profile stores roll back only the newly prepared
  record; an existing same-store registration is never removed.
- [x] Terminal in-memory task entries do not permanently consume the registration cap.
- [x] Schedule success is audited only after durable admission succeeds.
- [x] Rollback and terminalization failures are surfaced; failure audit cannot silently
  coexist with a task left in `prepared` state.
- [x] Executor, agent-batch, worker-launch, and in-memory completion guards propagate
  durable compensation failures instead of discarding store errors.
- [x] Unix workers run outside the caller's process group and are reaped.
- [x] Reaper wait errors cannot retain an owned child indefinitely.
- [x] Terminal durable history is retained under a deterministic bounded policy that
  audits eviction and never evicts active work, so completed history cannot permanently
  block new admission.
- [x] Durable record and lock reads use no-follow opens plus stable file identity checks;
  path replacement cannot bypass serialization or parsing.
- [x] Focused concurrency, cap, rollback, and worker-lifecycle tests pass.

### Affected Areas

`src/daemons/task.rs`, `src/daemons/workload.rs`, `src/tools/core.rs`,
`src/tools/executor.rs`, `src/agent/loop.rs`, and durable-task tests.

### Validation Gates

Focused daemon/durable-task tests, `task test`, `task check`, and `task coverage`.

## Final Lock And Reconciliation Review Remediation (2026-07-15)

### Scope

Prevent durable workload and shared daemon lock domains from splitting when visible
lock paths are replaced, reject link/open identity races, and make a process loss after
a stale worker is claimed for reconciliation resume to one terminal outcome.

### Acceptance Criteria

- [x] Per-task and admission locks retain an independently reachable persistent inode
  anchor outside the replaceable task directory; replacing a visible lock path or the
  complete task directory cannot admit a second cross-process owner.
- [x] Task locking uses a fixed, deterministic stripe set so unique, failed, missing,
  rolled-back, and evicted task IDs cannot create lifetime-unbounded visible locks,
  anchors, inode use, or directory-scan work.
- [x] Shared cron, curator, audit, and delegation-record locks use no-follow opens,
  opened/path identity checks, and a persistent anchor so replacement cannot create a
  parallel writer or redirect the lock through a symlink race.
- [x] Task and shared lock validation rejects every Windows reparse-point file type,
  not only paths reported as symbolic links.
- [x] Protected state reads, writes, enumeration, migration, removal, and lock-link
  operations resolve relative to retained directory capabilities. A pure transition
  aborts when detachment is known before commit; after an external effect, the paired
  transition commits atomically to the original directory and reports attachment loss.
- [x] Startup performs bounded, streaming cleanup of untouched legacy task locks.
  Delegation per-ID locks require an explicit offline migration: the operator stops and
  disables every prior nib binary, then runs
  `nib doctor --fix --confirm-no-legacy-processes`. Ordinary record operations never
  delete or recreate legacy artifacts.
- [x] A genuinely new delegation-record namespace is no-replace-published by the
  current build with a versioned native-origin receipt already inside it. An existing
  clean but unmarked namespace, a pending/rejected migration receipt, or legacy state
  introduced after a completed epoch fails closed with the same actionable doctor
  instruction.
- [x] Native-origin staging creation and no-replace publication use one absolute
  deadline through the final namespace mutation, parent sync, identity reopen, and
  visibility check. An expired publication never reports success and leaves exact
  receipt-bound state recoverable. Doctor preserves unmarked, foreign-identity, or
  extra-content staging byte-for-byte; an operator must inspect and explicitly remove
  that exact ambiguous directory before retrying.
- [x] The same absolute deadline covers delegation setup from capability-relative
  `.nib`, lock-parent, and anchor-parent creation through visible lock publication,
  exact anchor linking, parent sync, identity reopen, and final visibility. Expiry wins
  over success and retains any exact partial lock pair for a bounded fresh retry.
- [x] Records initialization derives its effective absolute deadline once and shares
  that exact instant across native setup, authorization, and legacy migration. A child
  directory created before expiry but not yet parent-synced is incomplete: a fresh
  bounded retry must capability-reopen the exact child and sync its parent before it can
  report success.
- [x] Deadline-aware shared file locks, including session cancellation audit and
  managed-process scope storage, use the same retained-capability setup for lock and
  anchor parents, visible file, exact link, durable sync, reopen, and final visibility;
  process-store namespace creation and maintenance consume the caller's same deadline.
- [x] The doctor confirmation is the authoritative external quiescence attestation. Its
  pending receipt is bound to the exact records-directory identity and exact legacy
  artifact manifest, authorizes only bounded doctor cleanup/resume, and becomes an
  ordinary-operation authorization only after cleanup is durably complete. Crash retry
  is idempotent; changed, ambiguous, or live state is preserved and needs a fresh
  confirmation.
- [x] Deterministic child-process regressions hold each original lock while replacing
  visible paths and prove contenders block or fail closed until identity is restored.
- [x] Every durable execution has a persisted generation included in terminal and
  schedule delivery keys. Reused IDs produce distinct evidence, while pre-generation
  records receive a deterministic persisted `legacy-*` generation and compatible
  deduplication of legacy session/audit evidence.
- [x] A durable task already in `reconciling` is resumed after process loss and reaches
  one terminal state without duplicating its session observation or audit delivery.
- [x] Terminal and schedule workers publish cancellation, wake/start, completion, and
  failure session/audit effects only inside the same owned critical section as the
  corresponding durable transition; a revoked worker cannot publish late effects.
- [x] Worker lease fencing remains authoritative throughout resumed reconciliation,
  including real child loss before and after terminal or schedule delivery.

### Affected Areas

`src/daemons/state.rs`, `src/daemons/workload.rs`, `src/daemons/task.rs`,
`src/daemons/cron.rs`, `src/daemons/curator.rs`, `src/tools/core.rs`,
`src/tools/delegation.rs`, daemon and durable-task tests, and workload validation
evidence.

### Implementation Evidence

- `src/daemons/state.rs` owns retained `StableDirectory` capabilities, capability-relative
  atomic publication, bounded streaming directory scans, and anchored shared locks.
- `src/daemons/workload.rs` owns capability-bound fixed task/admission stripes, global
  legacy cleanup, persisted execution generations, paired worker transitions, and
  resumable reconciliation.
- `src/daemons/task.rs` and `src/tools/core.rs` correlate terminal and schedule session,
  tool-call, and audit evidence by execution generation with an exact legacy fallback.
- `src/tools/delegation.rs` places record locks in a fixed stripe namespace outside the
  replaceable records directory. It atomically publishes native-origin namespace
  evidence and gates the one old per-ID cleanup through an exact, versioned offline
  doctor epoch rather than attempting live coexistence with prior binaries.
- `src/doctor.rs` exposes the explicit
  `--fix --confirm-no-legacy-processes` operator workflow; the confirmation text defines
  the required stop/disable precondition and no environment-only bypass exists.

### Validation Evidence

- State capability tests cover known pre-commit detachment, handle-bound no-replace
  publication for retained-file and proven-missing expectations, paired post-effect
  commit, bounded scans, and lock-parent replacement.
- Workload tests cover fixed control identities, untouched legacy cleanup and live-owner
  failure, original-directory paired commit, terminal/schedule ID reuse, deterministic
  legacy migration, reconciliation loss before/after delivery, and actual killed worker
  publication before/after effects with late-owner fencing.
- Delegation tests cover clean-unmarked rejection, native-origin receipt publication,
  deadline-bound native staging/publication, byte-exact hostile staging preservation,
  exact-receipt staging resume, every directory/file/link boundary in bounded lock
  setup, retained-pair retry after expiry before final sync, parent-fsync retry for an
  already-created exact child, one aggregate setup-and-migration deadline, bounded
  shared session/process lock setup, bounded attested migration, post-epoch legacy
  injection, crash/quarantine retry, and a prior-version child paused after opening the
  exact legacy inode but before locking it.
  The child state is preserved by ordinary operations, and doctor cleanup is accepted
  only after the child exits. A separate child writer covers complete records-directory
  replacement; Cron covers directory replacement after the scheduled job effect.

### Validation Gates

- [x] Focused bounded-artifact, lock replacement, process-loss reconciliation, fenced
  terminal/schedule side-effect, generation-reuse, and idempotent-delivery regressions.
- [x] `cargo fmt --check`.
- [x] `cargo clippy --all-targets --all-features -- -D warnings`.
- [x] Repository-wide `task test`, `task check`, and `task coverage` final local gates.
- [x] Isolated Linux release-binary smoke.
- [x] Windows CI `task test` and Windows/macOS runtime smoke final platform gates.

## Final Memory And Pin Linearization Review (2026-07-15)

### Scope

Make discrete memory persistence use the same no-follow, identity-bound capability
model as other profile state, and linearize curator pin updates with every destructive
memory or managed-skill cleanup decision.

### Acceptance Criteria

- [x] Memory-store reads retain the opened handle, reject symlinks and every Windows
  reparse-point type, and compare it with a no-follow path re-open before accepting data.
- [x] Memory-store locking has a persistent identity anchor outside the replaceable
  state path, and atomic publication resolves relative to a retained directory capability.
- [x] Memory cleanup re-reads pins while holding the pins lock and retains that lock
  through the memory update, so a successful concurrent pin cannot precede deletion.
- [x] Managed-skill cleanup re-reads pins under the pins lock immediately before removal;
  the existing skill-usage lock and pins lock have one documented, deadlock-free order.
- [x] Deterministic barrier tests race pinning against memory and skill cleanup and prove
  every pin that returns success before the destructive commit preserves its target.
- [x] Local file, lock, and parent-directory replacement regressions prove memory
  reads/writes and curator deletion fail closed without losing authoritative state.
- [x] Windows runtime reparse regressions prove the same behavior on Windows.

### Affected Areas

`src/session/memory.rs`, `src/daemons/curator.rs`, shared state/lock helpers, profile
initialization, and focused memory/curator process tests.

### Validation Gates

Focused memory identity/capability and pin-linearization tests; `task test`, `task check`,
`task coverage`, Windows CI `task test`, and isolated release-binary smoke.

### Local Validation Evidence

`cleanup_old_memory_at_with_hook` holds the pins lock across the memory-store update.
`cleanup_old_skills_at_with_hook` establishes the shared pins-before-skill-usage order
and retains both locks through removal. Barrier regressions prove a memory or skill pin
that commits before cleanup acquires the pins lock preserves the target. The focused
curator unit suite passes 19/19. `MemoryStore` now uses `with_file_lock_in`,
`StableDirectory::open_read`, post-read file identity verification, and capability-
relative atomic publication; its focused suite passes 9/9, including file replacement
and parent detachment. Fresh local aggregate gates and Linux smoke passed on
2026-07-16; Windows runtime evidence remains open.

## Final Persistence Integrity Remediation (2026-07-15)

### Scope

Make memory, pin, durable-task, and managed-skill persistence conditional on retained
prior identities; preserve newer durable state after post-effect conflicts; linearize
skill tracking with cleanup; and bound both temporary artifacts and recursive cleanup.

### Acceptance Criteria

- [x] Memory and curator-pin read-modify-write paths retain the opened prior file or
  proven absence and verify it immediately before atomic publication. Public memory
  snapshot saves take a mutable snapshot, use its legacy-compatible persisted revision
  as the CAS token, reject stale or overflowing revisions, and write the committed
  revision back into the caller. Legacy files without a revision load at revision zero.
- [x] Durable-task pure and post-effect commits are conditional on the record that was
  read. A post-effect mismatch preserves the newer record, returns non-success, and is
  reconciled without duplicating session or audit delivery.
- [x] Each due cron occurrence persists its cadence advance and a pre-effect
  `effect_unknown` claim before invoking the callback, then persists a completed, error,
  or audit-failure outcome. Restart never replays an unresolved occurrence and never
  hides a skipped or uncertain effect behind `next_run` alone.
- [x] Managed-skill tracking and cleanup share one anchored lock and retained root
  capability; deletion conditionally detaches the selected directory into quarantine
  and verifies the moved identity before recursive cleanup.
- [x] Unix cleanup conditionally detaches the exact selected file, symlink, or tree into
  quarantine, preserves ambiguous replacements, and reports unverified residual
  physical cleanup. Exact unlink after malicious same-UID pathname replacement is not
  claimed because that peer model is outside nib's isolation boundary.
- [x] Managed-skill cleanup enforces entry, depth, and aggregate-name bounds, holds
  decision locks only through quarantine, and never follows links or reparse points.
- [x] Shared atomic persistence uses deterministic per-destination or per-lock temporary
  artifacts with bounded stale-crash recovery. Recovery conditionally quarantines only
  an identity-matching, unlocked pre-evacuation temp; unjournaled prior or quarantine
  artifacts are preserved and fail closed, including target-missing plus prior-present.
- [x] Deterministic same-process and child-process barriers cover file substitution at
  memory/pin/task commit, task substitution after effects, recent re-tracking, directory
  substitution at deletion, process-kill temp recovery, and oversized skill trees.
  `real_child_memory_commit_barrier_and_fsync_crash_recovery`,
  `real_child_task_commit_barrier_and_fsync_crash_recovery`, and the shared atomic crash
  matrix cover the concrete persistence adapters and both crash-recovery outcomes.

### Affected Areas

`src/daemons/state.rs`, `src/daemons/workload.rs`, `src/daemons/cron.rs`,
`src/daemons/curator.rs`, `src/session/memory.rs`, durable delivery/reconciliation callers,
and focused process tests.

### Validation Gates

Focused expected-identity, post-effect reconciliation, managed-skill quarantine,
bounded-recursion, and crash-recovery tests; `task test`, `task check`, `task coverage`,
Windows CI `task test`, and isolated release-binary smoke. Local aggregate validation
passed 772 tests at 83.94 percent coverage (53,734/64,015), plus the locked build and
Linux release/PTY smoke, on 2026-07-16. Windows-only runtime criteria remain unchecked
until executed.
macOS publication uses the native no-replace rename path and fails closed if its
identity checks do not hold; macOS runtime execution also remains unchecked.

## Hosted Windows Persistence Remediation (2026-07-20)

### Scope

Repair the Windows-only persistence primitives exposed by the first hosted runtime
execution. Preserve direct-child, handle-relative, no-replace publication; accept valid
DOS short aliases without accepting reparse traversal; and let an observation-only
visible-directory handle coexist with the retained DELETE-capable directory handle.
Keep durable ownership receipts share-compatible, then acquire and identity-check a
fresh DELETE-capable file object for destructive cleanup. Separate reopenable namespace
directories from short-lived, consumed directory-mutation capabilities. Verify bytes
through the already-open publication handle while its Windows byte-range lock is held,
and canonicalize containment roots before comparing them with canonical child paths.
Make persistent daemon-lock acquisition retry a concurrent final-owner cleanup that
temporarily removes both the visible lock and its anchor. Persist canonical worktree
ownership roots across DOS-short aliases, and enumerate retained DELETE-capable
directories through the existing handle rather than opening a conflicting second one.

### Acceptance Criteria

- [x] File publication, directory quarantine, and recursive-removal quarantine share one
  native `NtSetInformationFile(FileRenameInformation)` helper with the retained parent
  as `RootDirectory`, a validated one-component destination, and replace disabled.
- [x] NT failures are converted to ordinary Windows errors and an existing destination
  preserves both source and target.
- [x] Windows visible-directory observation requests only directory-list and attribute
  access and shares read, write, and delete. Ordinary capabilities detect namespace
  replacement by identity, while explicitly owned deletion capabilities may pin it.
- [x] Directory ownership receipts use a separate observation file object. Empty-tree
  cleanup consumes and closes the strong directory capability, while recursive cleanup
  verifies a fresh DELETE-capable handle against the receipt before quarantine or delete.
- [x] Ordinary and long-lived child directories do not request DELETE access, so task,
  session, lease, lock, and Git registration namespaces can be reopened concurrently.
  Rename and empty-directory removal require an explicit owned child capability.
- [x] Focused source/target collision, present/missing publication, directory quarantine,
  namespace reopen, receipt coexistence, and handle-lifetime deletion regressions are
  present and cross-compile.
- [x] Windows publication verification reads bytes through the locked publication handle
  while independently proving that the destination path still names that file.
- [x] Canonical child containment accepts the runner's valid DOS-short project root
  without weakening component-level reparse rejection.
- [x] Concurrent first-use daemon-lock acquisition tolerates another owner's exact
  visible-lock/anchor cleanup without losing serialized pin updates.
- [x] Durable worktree ownership records remain valid when reservation, restart, and
  cleanup use equivalent DOS-short and canonical project-root spellings.
- [x] Bounded nested-tree scans operate through a retained DELETE-capable directory
  handle without triggering a Windows sharing violation.
- [x] The full Windows job passes with its default `C:\Users\RUNNER~1` temporary root.

### Affected Areas

`src/fs_security.rs`, `src/daemons/state.rs`, `src/daemons/curator.rs`, containment
callers, `src/sandbox/worktree.rs`, Windows-only filesystem/state tests, and the
`windows-sys` feature surface in `Cargo.toml`.

### Validation Gates

Windows-target `task check:all-targets`, the focused Windows runtime
regressions, `task test`, `task check`, and the hosted Windows build and smoke job.

### Validation Evidence

The full Windows target graph cross-compiles locally with the WDK filesystem and Win32
I/O bindings enabled. Native behavior remains unchecked until the hosted job executes;
the job deliberately retains its default short-path environment as the final regression.

## Hosted Windows Handle Follow-up (2026-07-20)

### Scope

Preserve canonical durable paths while adapting them at the external Git command
boundary to the non-verbatim spelling supported by Git for Windows. Make retained state
directory and file capabilities share deletion so replacement-race tests can mutate the
visible namespace while the original identity remains pinned. Read retained locked
files through their existing handle, and place cleanup-lease ownership outside the
bounded JSON payload range so Windows readers can validate live leases without opening
a conflicting byte range.

### Acceptance Criteria

- [x] Durable managed-worktree records retain canonical paths, while every `git
  worktree add` target uses an equivalent non-verbatim Windows path.
- [x] Windows state directory and child capability handles opt into read, write, and
  delete sharing without weakening no-follow or identity checks.
- [x] Retained worktree ownership and ref receipts are verified with positional reads
  through the existing handle while byte-range ownership is held.
- [x] Live cleanup-lease JSON remains readable during bounded state accounting and
  mutation, and atomic recovery validates an identity-equal target through the already
  locked transaction handle.
- [x] Canonical-equivalent Windows session-store paths compare by filesystem identity
  rather than raw DOS-short versus verbatim spelling.
- [x] The full hosted Windows job passes under its default `C:\Users\RUNNER~1`
  temporary root.

### Affected Areas

`src/fs_security.rs`, `src/daemons/state.rs`, `src/sandbox/process.rs`,
`src/sandbox/worktree.rs`, `src/integrations/worktree.rs`, `src/tui/mod.rs`, and focused
Windows filesystem, persistence, and worktree regressions.

### Validation Gates

Focused retained-handle, cleanup-lease, canonical-path, and worktree tests;
Windows-target `task check:all-targets`; `task test`; `task check`; `task coverage`; and
the hosted Windows build and smoke job.

### Local Validation Evidence

The full state, managed-worktree, session-worktree, process-scope, TUI, and delegation
suites pass on Linux. Windows all-target and all-feature cross-checks include the native
sentinel lock, share-mode, identity, positional-read, and real Git-boundary regressions.
Native Windows execution remains open until the hosted job passes.

## Hosted Windows Runtime Follow-up II (2026-07-20)

### Scope

Accept Git for Windows registration pointers whose parent is the same trusted
`worktrees` directory under an equivalent slash, verbatim-prefix, or DOS-alias spelling,
without replacing direct-child and no-reparse validation with lexical trust. Ensure all
retained observation and capability directory handles share namespace deletion where no
active descendant lock pins the tree. Treat Windows' stronger denial of a parent move
around a live byte-range lock as a valid fail-closed result and prove the original lock
domain remains intact. Keep durable ownership and ref payload validation readable while
their ownership protocol is active, without weakening live-owner exclusion.

### Acceptance Criteria

- [x] Managed worktree capture matches the reported registration parent to the trusted
  canonical namespace using only equivalent Windows spellings, then rebuilds and opens
  exactly one normal direct-child component beneath the trusted parent.
- [x] Equivalent Windows path spellings do not cause worktree creation, integration,
  compensation, delegation, or MCP flows to reject a valid Git registration.
- [x] Retained state and capability directory handles permit visible namespace
  replacement when the tree has no active descendant lock; a live Windows lock may
  instead pin the parent and must preserve the original namespace and state.
- [x] Durable ownership and ref receipts remain bounded and readable while ownership is
  held, and a live writer is still excluded from recovery or destructive cleanup.
- [x] The exact PR revision passes the full hosted Windows job under its default
  `C:\Users\RUNNER~1` temporary root.

### Affected Areas

`src/fs_security.rs`, `src/daemons/state.rs`, `src/daemons/curator.rs`,
`src/sandbox/worktree.rs`, `src/session/mod.rs`, native integration fixtures, focused
Windows filesystem/worktree regressions, and the hosted Windows job.

### Validation Gates

Focused registration-identity, namespace-replacement, and retained-receipt regressions;
Windows-target `task check:all-targets`; `task test`; `task check`; `task coverage`; and
the hosted Windows build and smoke job.

### Local Validation Evidence

The full host suite passes 614 library tests plus all binary, integration, and
documentation suites. Runtime coverage is 83.92 percent (56,345/67,143), release build
and managed-process smoke pass, and the Windows all-target/all-feature graph
cross-compiles the native path, range-lock, marker-identity, and replacement-contract
changes. Equivalent-path behavior and the remaining native contracts await the exact
hosted Windows run.

## Hosted Durable Schedule Remediation (2026-07-20)

### Scope

Prevent a due schedule from failing when its originating agent run still owns the
session run lease. Defer the wake without an arbitrary timeout, keep durable ownership,
heartbeat, and cancellation checks active, then retain the acquired session lease
through wake publication and scheduled execution. Bind that lease and execution to the
task's exact profile session store when multiple profiles share one workspace root.

### Acceptance Criteria

- [x] A due schedule waits behind an active session run without publishing `timer_fired`
  or setting an active occurrence.
- [x] A deferred schedule remains heartbeat-active, cancellable, and worker-lease
  fenced; cancellation publishes no late wake or scheduled-run outcome.
- [x] After the active run releases, the worker retains the exact acquired session run
  lease through wake publication and scheduled execution, producing one terminal
  occurrence. Ordinary concurrent agent-loop callers still fail immediately.
- [x] A non-default profile schedule verifies the lease against its canonical session
  directory and keeps execution and cancellation reconciliation in that store, even
  when another profile has the same session ID and workspace root.

### Affected Areas

`src/daemons/workload.rs`, `src/agent/loop.rs`, `src/session/mod.rs`, and focused durable
schedule regressions.

### Validation Gates

Focused release-to-completion, cancellation-before-wake, and same-root profile-isolation
regressions; `cargo fmt --check`; Clippy with warnings denied; spec integrity;
`task test`; `task check`; `task coverage`; and the hosted Linux, Windows, and macOS
jobs.

### Local Validation Evidence

Deterministic workload tests hold a real session run lease and prove both deferred
single completion after release and cancellation without `timer_fired`. A two-profile
regression holds the default profile's same-ID lease while the non-default schedule
completes only in its own session store. The full host suite passes 614 library tests
plus all binary, integration, and documentation suites, twelve consecutive durable
integration stress repetitions pass, and runtime coverage is 83.92 percent
(56,345/67,143).

## Hosted Cross-Platform Runtime Follow-up III (2026-07-20)

### Scope

Treat an identity-distinct target and prior artifact as an in-flight atomic publication
when the temporary pathname has already moved to the target and the writer still owns
that target's advisory lock. Preserve fail-closed ambiguity once no live writer owns the
published target. Reuse the shared Windows canonical-path validation for skill sources
so an equivalent DOS short alias is not misclassified as a symlink, while retaining
component-level reparse rejection. Make skill diagnostics path-separator portable. On
Windows, do not report bounded command cleanup complete until process handles captured
from the Job Object have entered the signaled state as well as job accounting reaching
zero active processes. Keep the Linux owner-loss coverage fixture deterministic under
instrumentation by synchronizing its escaped-descendant probe explicitly instead of
relying on a fixed child lifetime.

### Acceptance Criteria

- [x] Non-strict recovery skips an identity-distinct target/prior pair while the moved
  target is locked by a live atomic writer, preserving both paths and their bytes.
- [x] Strict recovery waits within its existing bound for a moved live writer, then
  rechecks path identity and accepts the writer's completed prior cleanup; an unlocked
  identity-distinct target/prior pair remains an ambiguity error.
- [x] Skill installation accepts an existing real directory reached through an
  equivalent Windows DOS short alias while symlinks and Windows reparse points remain
  rejected.
- [x] Skill-list malformed-manifest diagnostics are asserted with the host path
  separator.
- [x] Windows bounded-command cleanup waits for the captured Job Object process handles
  to become signaled before returning and still verifies zero active job processes.
- [x] The Linux owner-loss recovery fixture uses an explicit release barrier and still
  proves that an escaped descendant cannot survive namespace recovery.
- [x] The exact PR revision passes the full hosted Linux, macOS, and Windows jobs.

### Affected Areas

`src/daemons/state.rs`, `src/fs_security.rs`, `src/skill_cmd.rs`,
`src/sandbox/windows_job.rs`, focused atomic-recovery and Windows runtime regressions,
`tests/managed_process_supervisor_recovery.rs`, and the hosted CI matrix.

### Validation Gates

Focused moved-publication recovery and skill-command regressions; Windows-target
`task check:all-targets`; `task test:durable`; `task test`; `task check`;
`task docs:check`; `task coverage`; managed-process smoke; and the exact-revision hosted
Linux, macOS, and Windows jobs.

### Local Validation Evidence

Deterministic atomic-recovery regressions cover the live moved-publication handoff,
bounded strict timeout, unlocked ambiguity, and namespace retry after prior cleanup.
The full host `task check` passes 617 library tests, 62 binary tests, and every integration
and documentation suite; twelve consecutive `task test:durable` repetitions pass.
Instrumented validation passes at 83.98 percent runtime line coverage (56,591/67,385),
including the release-barrier owner-loss regression. The Windows all-target graph
cross-compiles, and the release build plus managed-process owner-loss smoke pass.
Independent spec-compliance and code-quality reviews report no remaining findings.
Native Windows short-alias and Job Object execution and the exact-revision hosted matrix
remain open.

## Hosted Windows Agent Stack Follow-up IV (2026-07-21)

### Scope

Keep the large agent-loop state machine off the platform-limited caller stack for both
cancellable and ordinary runs. The shared runtime wrapper must heap-pin the inner loop
before either branch awaits it, so CLI entrypoints do not depend on the operating
system's main-thread stack reservation. Exercise the existing durable CLI workflows
under a deterministic 1 MiB child-process stack budget on Linux while retaining native
Windows execution as the final platform gate. macOS retains its native stack limit because
Darwin rejects the synthetic 1 MiB `RLIMIT_STACK` during child setup. On Windows, launch
detached durable workers without inheriting ambient caller handles, so a caller waiting
for captured CLI output observes EOF when the CLI exits rather than when background work
finishes.

### Acceptance Criteria

- [x] Cancellable and non-cancellable agent runs heap-pin the inner future before awaiting it
  without changing cancellation precedence, run-lease verification, or reconciliation.
- [x] All four durable CLI integration workflows complete under a 1 MiB main-thread
  stack budget instead of aborting before their first tool result.
- [x] The canonical Task gates and Windows all-target graph remain green.
- [x] Windows durable-worker creation inherits no ambient capture handles while preserving
  its environment, working directory, detached execution, and durable PID publication.
- [x] A captured `nib run` invocation returns before its long-running durable terminal job,
  allowing a later `nib task cancel` process to cancel and reconcile that job.
- [x] The exact PR revision passes the full hosted Linux, macOS, and Windows jobs.

### Affected Areas

`src/agent/loop.rs`, `src/daemons/workload.rs`, Windows durable-worker process creation,
`tests/durable_tasks.rs`, durable task validation evidence, and the hosted CI matrix.

### Validation Gates

The constrained-stack `task test:durable` regression; cancellation-focused agent-loop
tests; `task test`; `task check`; `task docs:check`; Windows-target
`task check:all-targets`; and the exact-revision hosted Linux, macOS, and Windows jobs.

### Reproduction Evidence

Hosted Windows run `29792159373` passed delegation and reached `tests/durable_tasks.rs`,
where all four spawned `nib run` processes printed their session headers and then
aborted with a main-thread stack overflow. The same four failures reproduce locally
with a 1 MiB stack limit, while all four pass with a 2 MiB limit. The ordinary CLI path
previously awaited `run_agent_loop_inner` directly; it now heap-pins that future like the
cancellation-configured branch. The constrained regression applies the 1 MiB limit to
every Linux `nib` child and its inherited detached worker, and passes all four workflows.
Hosted macOS run `29793559369` rejected that synthetic limit with `EINVAL` during
`pre_exec`, so macOS retains its native limit while hosted Windows provides the native
1 MiB platform gate. That run confirms the stack repair: three Windows durable workflows
pass and the cancellable workflow reaches `nib task cancel`. Its remaining failure occurs
because the standard Windows `Command` spawn inherits the captured parent pipe handles;
the caller therefore waits for the fixture's 30-second worker before observing CLI EOF,
then finds the task already completed. The Windows worker launcher now uses
`STARTUPINFOEXW` with an explicit inheritable-handle allowlist containing only separate
`NUL` handles for stdin, stdout, and stderr. Ambient caller handles are excluded while the
child still inherits its environment, receives the intended working directory, and remains
detached; the captured-output regression remains unchanged for hosted verification.
Compiler type-size inspection measures the inner future at 26,440 bytes while the patched
runtime wrapper is 3,584 bytes and the public loop future is 4,856 bytes. `task check`,
`task test:durable`, documentation integrity, and the Windows all-target graph pass.
Independent diagnosis and final code-quality review report no remaining blocking findings.

## Hosted Windows Gateway Timing Follow-up V (2026-07-21)

### Scope

Remove host-speed assumptions from the gateway serialization regression without weakening
its concurrency contract. Successful mock gateway progress may use a bounded 30-second test
budget, while the short negative probe must still prove that a second same-conversation
dispatch cannot enter the agent loop before the first releases its durable session lock.
Production gateway timeouts and locking behavior remain unchanged.

### Acceptance Criteria

- [x] The gateway serialization regression retains its same-conversation exclusion,
  distinct-conversation progress, stream-backpressure, and final prompt-order assertions.
- [x] Every positive-progress phase uses one explicit 30-second test budget while the
  250 ms same-conversation no-entry probe remains unchanged.
- [x] The exact PR revision passes the full hosted Linux, macOS, and Windows jobs.

### Affected Areas

`src/integrations/gateway.rs`, hosted Windows timing evidence, and the CI matrix.

### Validation Gates

`task test`; `task check`; `task docs:check`; Windows-target `task check:all-targets`; and
the exact-revision hosted Linux, macOS, and Windows jobs.

### Reproduction Evidence

Hosted Windows run `29795887116` finished its library suite with 542 passing tests and one
failure: `same_conversation_runs_serialize_while_distinct_conversations_progress`
exhausted only its five-second post-drain completion wait. Every preceding protocol
assertion passed, and the same test passed in the immediately preceding Windows run. Across
those two runs, the neighboring two-dispatch gateway test slowed from 4.23 to 7.37 seconds
and the concurrency test moved from a 7.63-second pass to a 13.86-second failure.
Independent reviews found no lost wake or lock-ownership defect. The regression now uses a
30-second budget for bounded successful progress while preserving the 250 ms exclusion
probe and all state assertions.

## FT-019 Windows Root-Future Stack Follow-up (2026-08-30)

### Scope

Restore caller-stack independence after the FT-019 agent state machine enlarged the
public runtime wrappers. The CLI, durable task worker, and subagent worker must submit
their owned root futures to a Tokio worker with an explicit stack budget instead of
polling those futures on the platform-limited process main thread. The existing inner
future boxing, cancellation ordering, workload leases, reconciliation, and public output
contracts remain unchanged.

### Acceptance Criteria

- [x] `nib run` polls the owned agent root future on a configured runtime worker with a
  4 MiB stack while its main thread blocks only on the worker join handle.
- [x] Durable `task-worker` and `subagent-worker` entrypoints use the same root-future
  boundary, including scheduled agent execution and optional session-lock policy scope.
- [x] Returned worker join failures use bounded static errors without including panic
  payloads or changing successful agent and workload results. Process-global panic-hook
  behavior remains unchanged and is not part of this repair.
- [x] A deterministic unit regression proves the submitted root future is polled off the
  caller thread, and the four constrained-stack durable CLI workflows remain green.
- [x] The exact PR revision passes the full hosted Linux, macOS, and Windows jobs.

### Affected Areas

`src/agent/mod.rs`, `src/run.rs`, `src/main.rs`, `src/tools/delegation.rs`, this
development spec, the durable task regression, and the hosted native CI matrix.

### Validation Gates

The focused agent runtime-boundary unit regression; `task test:durable`; `task check`;
`task test`; `task docs:check`; Windows-target `task check:all-targets`; `task build`;
runtime coverage; and the exact-revision hosted Linux, macOS, and Windows jobs.

### Reproduction Evidence

Hosted Windows job `99333326200` at head
`8803408240d4c00ebc4027041c073c7f540360cc` passed all 1,057 library tests and all
91 binary tests, including the bounded legacy-lock migration regression, before every
`tests/durable_tasks.rs` workflow failed identically. Each spawned `nib run` process
printed its session header and then aborted with `thread 'main' has overflowed its
stack` before the first tool result. Linux's synthetic 1 MiB regression and the hosted
macOS job passed, so native Windows remains the authoritative platform gate. The root
CLI still calls `Runtime::block_on(run_agent_loop(...))`; the durable and subagent worker
entrypoints have analogous direct root-future polling boundaries. The repair therefore
belongs to production runtime dispatch rather than a relaxed fixture or a larger PE
main-stack reserve.

### Local Repair Evidence (2026-09-02)

`build_agent_runtime` now configures every Tokio worker with a 4 MiB stack, and
`block_on_agent_runtime_worker` submits the owned root future before the caller blocks
on its join handle. `nib run`, `task-worker`, and `subagent-worker` share that boundary;
the subagent path retains its captured optional session-lock policy. Deterministic unit
coverage proves the root future is polled on a different thread and that its returned
join error is bounded and excludes the panic payload. This does not suppress or replace
Rust's process-global panic hook.

`task test:durable` passed all four constrained-stack durable CLI workflows. The final
local `task verify` passed 1,061 library tests, 86 CLI tests, and 254 integration tests
with two explicitly ignored qualification tests. `task docs:check`, host and Windows
MSVC `task check:all-targets`, 85.87 percent runtime line coverage
(101,945/118,726), the locked release build, Linux interactive PTY smoke, and Linux
abrupt-owner managed-process smoke also passed. The exact hosted Windows, macOS, and
Linux revision remains the only open criterion in this follow-up.

## Superseded Historical Implementation Plan

1. Execute Windows short-alias, rooted rename, reparse/identity, and Windows/macOS
   daemon, curator, memory, and task runtime gates on their configured platforms.
2. Rerun the canonical Task gates and two-stage review before moving T004 to `done/`.

## Delegation Preparation Isolation Follow-up (2026-08-29)

Profile session initialization performed for a direct subagent spawn is a transactional
preparation, not an independently committed profile migration. Cleanup removes the
transaction's exact session leaf first. A marker, anchor, profile state directory, or
session directory is removed only when the preparation created that exact identity and it
is still empty; a concurrently committed session adopts and preserves the shared
infrastructure. Preparation create/cleanup operations serialize per canonical session
namespace, and ambiguous or replacement identities fail closed.

The read-only preflight also freezes the workspace-selected profile id together with its
sessions destination and runtime configuration. Child bootstrap reduces the copied
profile configuration to that selected id; it must not fall back to the global default
profile or inherit another profile's environment, skills, or state paths.

Acceptance and validation include deterministic A-prepares/B-commits/A-fails namespace
identity/byte preservation and a same-workspace non-default-profile child bootstrap with
distinct environment and skill fixtures.

Before the first fallback profile/session mutation, the durable preparation intent also
records a versioned namespace plan: the retained ancestor capability identity, the exact
post-worktree missing-component chain, the retained marker/anchor state, and
transaction-scoped marker bytes. Newly initialized session identity markers persist
those bytes before anchor publication. Restart cleanup first validates the complete
planned topology and marker/link identity; unrelated entries, substituted identities,
or mismatched bytes preserve the namespace fail closed. Directory removal is leaf-first,
empty-only, capability-bound, and checked against the same bounded cleanup deadline.

Validation additionally kills a subprocess immediately after the first planned directory
creation, marker durability, anchor publication, parent sync, and final visibility check.
Each restart must remove the exact partial transaction and no unrelated state. Separate
byte-snapshot regressions require hostile sentinels and ambiguous marker content to remain
unchanged.

The spawn-preparation workload intent is the write-ahead authority for the whole fallback
transaction, not only the session namespace. Its initial `planned` revision is published
and finalized before worktree reservation, child configuration, owner lease, or audit
mutation. It contains pre-generated worktree path/branch/base OID/durable receipt identity,
owner generation/lease, audit destination, and namespace nonce plan. Later revisions record
resource preparation, the exact planned session leaf, and its published identity. All
revisions and the session leaf use guarded atomic publication with exact receipt adoption;
an error receipt and any cleanup/finalization failure are durable reconciliation input and
must never be discarded.

Restart first authorizes and retains the exact Policy-B records-directory capability, then
recovers canonical, temporary, previous, and deletion-quarantine transaction state before
bounded intent enumeration. A subagent record supersedes an intent only when its canonical
identity and complete persisted generation, lease, worktree, child/audit session, pinned
audit target, committed handoff evidence, and durable execution evidence exactly match.
A merely published or registered `running` record remains a compensable preparation
artifact. Ambiguous, stale, unrelated, pending-handoff, or evidence-free records and
transaction artifacts preserve the intent and resources fail closed. Intent retirement is
last and a normal-path retirement error is surfaced and compensated rather than ignored.

One persistent bounded authority serializes the complete intent lifetime: the writer holds
the global migration fence and its fixed per-id record stripe, in canonical order, from
before `planned` publication through record commit or full compensation and intent
retirement. List/get reconciliation takes the same global fence before transaction
recovery or enumeration, so it cannot observe an evacuated live revision or compete with
the writer's deletion quarantine. The fence and stripe are fixed infrastructure rather
than per-spawn artifacts. Cleanup of session, owner, or worktree state is guarded before
and after each namespace mutation by the exact retained records-directory capability and
the bounded cleanup deadline; replacement preserves the transaction fail closed.

Planned session-leaf recovery is strict and receipt-driven. Before canonical
classification it resolves the deterministic temporary/previous/quarantine namespace
under the retained session-directory capability. Canonical and temporary links may be
finalized only when they identify the same unlocked file with the exact planned bytes;
live, mismatched, prior, or ambiguous state remains untouched. Successful reconciliation
requires the complete transaction-owned session namespace—including leaf artifacts,
identity marker/anchor, and empty created ancestors—to be finalized before deleting the
spawn intent.

Spawn compensation and intent retirement share the one absolute preparation deadline.
Expiry or an indeterminate constructor/cleanup result preserves the durable intent; it is
never retired merely because another cleanup step succeeded. Fresh bounded reconciliation
validates and removes the exact session/worktree/owner artifacts first and deletes the
intent last. Coverage includes session temporary and canonical publication boundaries,
worktree reservation, post-audit cancellation, and injected cleanup failures in both
synchronous and cancellable delegation paths.

TaskManager admission is paused until a durable execution handoff is acknowledged. Intent
revisions cover record publication, manager registration, and handoff proof; the matching
record then receives an exact committed marker before the start gate is released. Native
supervision uses a version-4 intent that preplans the exact cleanup lease and registration
nonce. The parent publishes a matching `Prepared`, `launch_committed=false` scope, and the
spawned supervisor's first workload-state operation exact-CASes its verified OS identity
into that scope under the same fixed process-scope lock used by restart. It does so before
reading the parent protocol, record, worktree, or other external launch resource. The
parent validates this already-persisted identity rather than publishing it. The supervisor
then keeps the real OS child launch gate closed while publishing nonce-bound `READY`; only an exact parent
`COMMIT` sent after durable intent/record handoff may advance that same scope to true,
release the worker, and produce `STARTED`. The version-4 preparation intent retains the
exact READY scope identity and authority for restart comparison. TaskManager release and
intent retirement follow `STARTED`. EOF, partial/malformed frames, identity mismatch,
timeout, or parent death before that point produce exact never-launched cleanup proof. On
restart, process state is opened through the retained authorized records capability and
classified once under the exact store lock after canonical/temp/previous plus scope- and
cleanup-lease-quarantine recovery. Only exact `subagent` key, generation, cleanup lease,
owner/backend/supervisor/child authority with `launch_committed=true` in a committed-compatible
state, or an exactly matching terminal cleanup proof, can supersede an intent. Prepared,
false, legacy missing flags, or mismatched authority fail closed. An uncommitted scope from
any preparation phase is cleanup authority and its exact scope and cleanup lease retire
before owner/audit/worktree/record/intent compensation. The legacy public `mark_running`
operation preserves its prior committed semantics with one atomic Prepared-to-Running
publication, so a version-2 Prepared record cannot be changed durably and then return an
error. TaskManager presence alone never supersedes the intent. Crash-boundary and
final-retirement regressions, including SIGKILL at scope/cleanup-lease quarantines and a
real-binary parent SIGKILL between `READY` and `COMMIT`, require that restart yields either a
valid launched workload or complete exact compensation, never an orphan `running` or
`recovery_required` record.

The supervisor's startup deadline is confined to the capability-only open,
self-registration, and `READY`/`COMMIT`/`STARTED` handoff. After `STARTED` succeeds, the
supervisor rebinds both the exact retained process-scope directory and its already-held
cleanup lease to long-lived authorities without reopening an ambient path or changing
their identities. Each later process-scope or cleanup-lease mutation receives a fresh
bounded operation deadline, so a worker may run longer than the startup lock timeout
without making terminal cleanup impossible. The startup deadline is never renewed before
commit. Production-path completion and cancellation coverage runs the gated worker beyond
that timeout and requires exact descendant cleanup plus retirement of the scope and lease.

Every version-4 intent revision and atomic prior/target successor retains the identical
preplanned cleanup lease and registration nonce. One shared structural and runtime validator
binds that plan to the READY scope and later observed scope before atomic recovery,
execution-evidence acceptance, or restart cleanup. Missing or changed nonce, changed lease,
or plan substitution is ambiguous authority: the transaction and all resources remain
byte-exact and no cleanup or supersession proceeds. Version-2/3 compatibility remains
plan-absent and fail closed; migration never invents version-4 authority.

The persisted preparation schema is version-exact. Version 2 permits neither a process
plan nor a `READY` snapshot in any phase and cannot treat matching committed process state
as handoff proof. Version 3 permits no plan and adds its exact legacy `READY` snapshot only
when advancing from `ManagerRegistered` to `HandoffProven`; version 4 requires the current
plan-bound representation. The matrix is enforced before encoding, atomic temporary or
previous recovery, revision adoption, evidence evaluation, and restart cleanup. Any
forbidden field or non-monotonic addition/removal leaves the entire transaction namespace
unchanged and requires explicit recovery rather than silently rewriting legacy authority.

An exact version-4 Prepared scope with no persisted supervisor, direct child, or cleanup
lease may be retired only while holding that self-registration lock. The resulting race is
linear: self-registration wins and restart aborts/reaps the exact persisted identity, or
retirement wins and a late supervisor exits on its failed CAS before accessing launch
resources. Production-linked SIGKILL coverage exercises both Prepared-before-spawn and
spawn-before-self-registration boundaries and requires scope retirement before any external
compensation, with no worker sentinel or resource reappearance after the late child exits.

## Historical Risks at This Stage

- Residual physical cleanup remains explicitly unverified when pathname ownership
  cannot be retained through unlink; hostile same-UID peers require an external
  account, VM/container, or privileged broker boundary.
- Non-Linux persistence, reparse, and cleanup paths remain unexecuted and may require
  platform-specific identity handling without weakening fail-closed preservation.


## Final Closure Evidence (2026-09-02)

This section supersedes earlier remaining-plan, current-risk, completion-state, and
native-evidence notes only where they described validation gates now executed. PR
[#25](https://github.com/skills-yaml/nib/pull/25) exact implementation run
[33683995100](https://github.com/skills-yaml/nib/actions/runs/33683995100)
passed the Validate, macOS Tests, and Windows Tests jobs for head
`c3b88564da4f6f654a8618e4fa544b353ece86f5` at clean merge checkout
`0479b72ad3d11fd7221632f042736b8489b6443b`. The matrix passed the complete
serial suites, Linux coverage at 85.87 percent (102,061/118,862), all native
all-target gates, exact release-binary qualification, and the Linux, macOS, and
Windows platform smokes.

The exact optimized binary hashes were
`e9b56b4c2b527ab04bd4e40932c83a632ae5bd5931010dee6152012b421e4276`
(Linux), `e7bbf6ea23d87a3e00b1447fc7880f2c93e6c67a27239f0068bcb599d18fb739`
(macOS), and
`e9250200aa0b06188e3e05d062ccd39115eb98311d0dc9b691cfdc5e9a324423`
(Windows). Local `task verify` also passed 1,062 library tests, 86 CLI tests,
every integration suite, and doctests during this reconciliation. All previously
open acceptance and validation items in this file are satisfied for its shipped
scope by this final matrix and the prior evidence recorded above.
