# T004: Profiles, Discrete Memory Store, and Maintenance Daemons (Cron/Curator)

**Status:** Development

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
- [ ] Windows and macOS runtime gates are green on the reconciled tree.

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
- [x] Startup performs bounded, streaming cleanup of untouched legacy task and
  delegation per-ID locks. A live legacy owner fails migration closed, and current
  stripe/admission controls are never interpreted as legacy artifacts.
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
  replaceable records directory and globally migrates the old per-ID namespace.

### Validation Evidence

- State capability tests cover known pre-commit detachment, handle-bound no-replace
  publication for retained-file and proven-missing expectations, paired post-effect
  commit, bounded scans, and lock-parent replacement.
- Workload tests cover fixed control identities, untouched legacy cleanup and live-owner
  failure, original-directory paired commit, terminal/schedule ID reuse, deterministic
  legacy migration, reconciliation loss before/after delivery, and actual killed worker
  publication before/after effects with late-owner fencing.
- Delegation tests cover bounded global legacy migration and a child writer paused while
  the complete records directory is replaced; Cron covers directory replacement after
  the scheduled job effect.

### Validation Gates

- [x] Focused bounded-artifact, lock replacement, process-loss reconciliation, fenced
  terminal/schedule side-effect, generation-reuse, and idempotent-delivery regressions.
- [x] `cargo fmt --check`.
- [x] `cargo clippy --all-targets --all-features -- -D warnings`.
- [x] Repository-wide `task test`, `task check`, and `task coverage` final local gates.
- [x] Isolated Linux release-binary smoke.
- [ ] Windows CI `task test` and Windows/macOS runtime smoke final platform gates.

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
- [ ] Windows runtime reparse regressions prove the same behavior on Windows.

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
- [ ] The full Windows job passes with its default `C:\Users\RUNNER~1` temporary root.

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
- [ ] The full hosted Windows job passes under its default `C:\Users\RUNNER~1`
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
- [ ] Equivalent Windows path spellings do not cause worktree creation, integration,
  compensation, delegation, or MCP flows to reject a valid Git registration.
- [x] Retained state and capability directory handles permit visible namespace
  replacement when the tree has no active descendant lock; a live Windows lock may
  instead pin the parent and must preserve the original namespace and state.
- [x] Durable ownership and ref receipts remain bounded and readable while ownership is
  held, and a live writer is still excluded from recovery or destructive cleanup.
- [ ] The exact PR revision passes the full hosted Windows job under its default
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

## Remaining Implementation Plan

1. Execute Windows short-alias, rooted rename, reparse/identity, and Windows/macOS
   daemon, curator, memory, and task runtime gates on their configured platforms.
2. Rerun the canonical Task gates and two-stage review before moving T004 to `done/`.

## Current Risks

- Residual physical cleanup remains explicitly unverified when pathname ownership
  cannot be retained through unlink; hostile same-UID peers require an external
  account, VM/container, or privileged broker boundary.
- Non-Linux persistence, reparse, and cleanup paths remain unexecuted and may require
  platform-specific identity handling without weakening fail-closed preservation.
