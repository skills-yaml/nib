# FT-015: Deep Sub-Agent Delegation

**Status:** Development
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Allow the primary `nib` agent to spawn, delegate to, and reconcile output from durably
tracked sub-agents operating asynchronously.

## Problem Statement
A single agent loop struggles with massive codebase refactors. It loses context quickly and cannot parallelize independent tasks.

## Goals
- Provide a `spawn_subagent(goal, worktree)` tool.
- Sub-agents operate in their own linked session and isolated git worktree.
- The primary agent acts as the Orchestrator, dispatching tasks and aggregating results.
- Implement reconciliation tools to merge the sub-agent's worktree back into the main branch once tests pass.

## Scope
- Create a `tools::delegation` module with `spawn_subagent` and `merge_subagent_worktree` tools.
- Implement `spawn_subagent` to initialize a new session and spawn a child Tokio task running `run_agent_loop` recursively.
- The `spawn_subagent` should return a job/session ID to the parent agent.
- Modify `ToolRegistry` to include the delegation tools.

## Acceptance Criteria
- `spawn_subagent` correctly creates a child session and executes a subagent run asynchronously in the background.
- `merge_subagent_worktree` correctly pulls/merges changes from a subagent's worktree.
- `task check` passes.
- Unit or integration tests demonstrate subagent spawning and completion.

## Affected Areas
- `src/tools/delegation.rs` (new module)
- `src/tools/registry.rs` (registering new tools)
- `src/tools/core.rs` (dispatching delegation tools)
- `src/tools/mod.rs` (exporting delegation)

## Validation Gates
- `task check`
- `task test`

## Reopened Audit (2026-07-15)

Scope: link parent/child sessions and worktrees, expose status/results, require
verification before merge, avoid nested worktree routing, and test reconciliation.

Affected areas: `src/tools/delegation.rs`, `src/daemons/task.rs`, worktree/session
models, registry/executor routing, and delegation tests.

Validation gates: spawn/status/message/verified-merge tests, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Create linked child sessions/worktrees, run child loops asynchronously, persist status
and results, support message/cancel, and require separately approved verification before merge.

### Acceptance Criteria

- [x] Spawn returns durable parent/child/session/worktree/branch identity.
- [x] Child completion, failure, cancellation, and runtime interruption reach terminal records.
- [x] Noninteractive children policy-approve only their plan; destructive/network
  actions remain denied even when loaded policy contains an allow rule.
- [x] Status/list/message/cancel operations validate scope and persist.
- [x] Merge requires audited successful terminal verification in the exact child worktree.
- [x] Merge and cleanup failures preserve terminal evidence; successful merge cleans
  the worktree before the record becomes `merged`.
- [x] Fresh local aggregate gates are green on the reconciled tree.
- [ ] Windows and macOS runtime gates are green on the reconciled tree.

### Affected Areas

`src/tools/delegation.rs`, `src/tools/executor.rs`, `src/daemons/task.rs`,
`src/sandbox/worktree.rs`, session state, and delegation tests.

### Implementation Evidence

`SubagentRecord`, `SubagentRunGuard`, and `VerificationEvidence` in
`src/tools/delegation.rs` provide durable reconciliation and verified merge input.

### Validation Evidence

Eleven scenarios in `tests/delegation.rs` cover spawn, completion/bounds, policy,
cancellation, interruption, symlinks/bounds, verification, backend failure, merge, and
the child allow-policy ceiling. The `src/tools/delegation.rs` cleanup-failure unit test
proves that `merged` is impossible until worktree cleanup succeeds.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
ownership, worktree, and platform gates below are authoritative for completion.

- [x] Full delegation integration target covers child allow-policy ceilings and
  worktree-cleanup failure reconciliation.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

Subagents run as Tokio child loops rather than separate OS services. Nested worktree
routing remains intentionally prohibited by scoped worktree ownership. The final
sentence of the earlier assessment is superseded by the ownership/worktree remediation
and FT-017 boundary below.

## Final Quality Review Remediation (2026-07-15)

### Scope

Keep generated `.nib` state out of commits, make merge execution bounded and
cancellable, abort conflicts cleanly, and persist/reconcile a two-phase merge state so
Git integration, cleanup, and durable status cannot silently disagree.

### Acceptance Criteria

- [x] Delegation staging excludes `.nib` even when the repository has no `.gitignore`.
- [x] Git add/diff/commit/merge/abort/cleanup commands have bounded, noninteractive execution.
- [x] Parent integration is serialized by a repository-scoped lock across subagent IDs.
- [x] Repository lock acquisition is time-bounded and proves the opened lock file still
  names the same no-follow regular file.
- [x] Verification is bound to an immutable staged child commit; any mergeable edit after
  verification fails closed and requires fresh verification.
- [x] Cancelling an in-flight merge leaves state that the next locked retry can abort and
  restore without resetting user changes.
- [x] Immediate failure recovery applies the same MERGE_HEAD/base/path ownership proof as
  retry recovery and never aborts an unrelated human merge.
- [x] nib never deletes an ambiguous live Git `index.lock`; ownership that cannot be proven
  fails closed with actionable recovery evidence.
- [x] A merge conflict restores the parent HEAD/index/worktree and records retryable evidence.
- [x] A missing pending worktree can fall back to parent verification only after the
  recorded child commit is proven already integrated; otherwise merge fails closed.
- [x] A persisted pre-merge state can reconcile success after post-merge cleanup or write failure.
- [x] Partial/cancelled worktree cleanup reconciles an already integrated commit without
  treating a leftover path as valid verification evidence.
- [x] Delegation record reads use stable no-follow file identity checks.
- [x] `merged` is written only after the branch is integrated and worktree cleanup succeeds.
- [x] Secret-exclusion, serialization, conflict, timeout/cancellation, missing-worktree,
  cleanup-failure, and two-phase recovery tests pass.

### Affected Areas

`src/tools/delegation.rs`, `src/sandbox/worktree.rs`, delegation records, and
`tests/delegation.rs`.

### Validation Gates

Focused delegation lifecycle/security tests, `task test`, `task check`, and `task coverage`.

## Persistent Merge-Lock Remediation (2026-07-15)

### Scope

Bind repository-wide merge serialization to a persistent lock identity that survives
replacement of the visible `.nib/subagents/.merge.lock` path or the replaceable
`subagents/` directory. Preserve bounded, cancellation-safe acquisition and the
existing merge/recovery ownership proofs.

### Acceptance Criteria

- [x] Every contender locks a persistent repository-owned anchor outside the
  replaceable records directory while retaining a visible no-follow lock path for
  validation and diagnostics.
- [x] Replacing the visible merge-lock path or the whole records directory cannot
  create a second live merge-lock domain.
- [x] A child-process regression holds the original lock, replaces each visible path,
  and proves a second process times out rather than entering merge/recovery.
- [x] Focused persistent-lock identity tests pass.
- [x] Fresh canonical local repository gates pass after the final ownership/worktree
  remediation.
- [ ] Windows and macOS runtime gates pass after the final ownership/worktree
  remediation.

### Affected Areas

`src/tools/delegation.rs`, delegation lock tests, lifecycle documentation, and final
validation evidence.

### Validation Evidence

`RepositoryMergeLock` locks `.nib/.subagents.merge.lock.anchor`, verifies that the
visible `.nib/subagents/.merge.lock` hardlink has the same opened identity, and checks
both identities again after bounded acquisition. The
`persistent_anchor_prevents_replaced_repository_lock_domains` child-process regression
covers intact contention, visible-path replacement, whole-directory replacement, and
restored reacquisition. The focused delegation unit suite passes 7/7 and the delegation
integration target passes 21/21.

### Validation Gates

Focused repository-lock unit/process tests, `task test`, `task check`, `task coverage`,
and the required two-stage final review.

### Risks

Hardlink creation and repair must fail closed when either path is a symlink or the
opened identities disagree. Anchor placement must not create a new writable scope or
weaken the existing project-root containment checks.

## Final Process-Loss Reconciliation Remediation (2026-07-15)

### Scope

Make a persisted `running` subagent recoverable after the process that owned its Tokio
task exits before `SubagentRunGuard::drop` can terminalize the record. Startup/status/
cancel paths must reconcile orphaned ownership without confusing process loss with
natural completion or leaving an untracked child.

### Acceptance Criteria

- [x] Each running subagent persists a bounded execution generation and owner lease that
  can be distinguished from a live current-process task handle.
- [x] Startup or the first status/cancel/list operation performs bounded reconciliation
  of running records whose owner is no longer live, reaching an explicit interrupted or
  failed terminal state with audit evidence.
- [x] A stale owner or late completion from an older generation cannot overwrite a
  reconciled or reused subagent record.
- [x] Cancellation never reports normal completion for an orphaned running record and
  never reports cancellation until stopped/terminal state is durably proven.
- [x] Owner-lease, merge-lock, and remaining precommit record paths reject every Windows
  reparse-point type and compare opened/path identities before use.
- [x] A real child-process kill/restart regression proves a persisted running subagent is
  reconciled after whole-process loss; in-runtime future abort coverage remains green.

### Affected Areas

`src/tools/delegation.rs`, task ownership/status inspection, session audit, delegation
record migration, and process-level delegation tests.

### Validation Gates

Focused process-loss, generation fencing, status/cancel, and audit tests; `task test`,
`task check`, `task coverage`, Windows CI `task test`, and isolated release-binary smoke.

### Validation Evidence (2026-07-15)

- `cargo test tools::delegation::tests --lib`: 14 passed, including real child-process
  owner loss, live-owner lease path replacement, stale generation fencing, legacy
  fail-closed handling, orphan cancellation, and lease cleanup.
- `cargo test --lib`: 384 passed, including MCP cancellation reconciliation and audit.
- `cargo test --test delegation dropping_the_runtime_cannot_leave_a_spawned_record_running`:
  1 passed.
- `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all
  -- --check` passed before the independent persistence lane began its concurrent
  `state.rs` API migration; that lane owns the current transient dead-code warnings.
- Windows reparse behavior is implemented through the central metadata predicate and
  capability no-follow/identity checks; Windows CI was not available on this Linux host.

## Final Ownership and Worktree Publication Remediation (2026-07-15)

### Scope

Close the remaining cross-process publication, owner-lease, and managed-Git gaps found
after process-loss reconciliation. Record transitions must be conditional on the exact
file generation that was read, owner reconciliation and artifact cleanup must remain
bounded under hostile lock/path state, and managed Git must not execute repository or
user-configured helpers while creating, verifying, merging, or compensating worktrees.

### Acceptance Criteria

- [x] Initial record creation publishes only when the destination is still missing, and
  every read-modify-write transition publishes only while the destination still has the
  exact opened identity that supplied the record; stale owner, cancellation, merge, and
  reconciliation writers cannot overwrite an externally replaced generation.
- [x] The shared atomic publication primitive performs its final identity/absence check
  after any precommit hook and uses an OS-safe conditional publication strategy rather
  than an unconditional overwrite rename.
- [x] Precommit record cleanup conditionally moves only the exact opened record into a
  no-replace quarantine and cannot detach a replacement installed before that
  namespace transition.
- [x] On Unix, record cleanup conditionally detaches the exact opened entry into
  quarantine, preserves ambiguous replacements, and reports unverified residual
  physical cleanup. Exact unlink after malicious same-UID pathname replacement is not
  claimed because that peer model is outside nib's isolation boundary.
- [x] Record-stripe and owner-lease namespace acquisition has a deterministic deadline;
  spawn, status, list, cancel, and shutdown cannot block indefinitely on a live holder.
- [x] Owner-lease reconciliation bounds both visible leases and persistent anchors,
  removes or reports anchor-only artifacts, and can distinguish a displaced visible path
  from a dead owner without creating a second live lease domain.
- [x] Managed Git disables system/global configuration, external attributes, hooks, and
  ambient executable helpers; rejects repository and per-worktree executable
  filter/diff/merge/credential/include configuration; and runs with a minimal non-secret
  environment under the existing timeout and managed-process containment.
- [x] Each real Git command performs bounded helper/configuration preflight immediately
  before launch and fails closed on observable changes. Git does not provide a portable
  immutable local-configuration snapshot; malicious same-UID mutation after preflight
  is outside nib's documented isolation boundary.
- [x] Initial worktree creation persists a receipt-ID-bound reservation before creating
  either artifact, CAS-attaches the staged filesystem identity before final publication,
  and retains the staged loose ref as a hard-link generation anchor. The branch anchor
  is durable before Git packed/target protocol locks are acquired. Those locks carry a
  receipt/reference/role marker and retained kernel lock, so restart removes only a dead
  matching owner and preserves live, foreign, or ambiguous locks. Compensation
  deletes only an owned ref that still has the expected object ID and anchor identity;
  pre-existing, replaced, or concurrently moved refs are preserved and reported. Before
  initial creation succeeds, symbolic `HEAD`, `HEAD^{commit}`, and the owned ref must
  still match the recorded object ID.
- [x] Later adoption of an existing branch at an identical object ID has a durable
  generational ownership link that distinguishes nib's branch from an unrelated ref
  created or replaced between runs. Adoption requires the CAS-retained ownership
  generation and rotates the persisted branch-ref identity and hard-link anchor together
  with its object ID. Prior-anchor retirement is a mandatory persisted phase resumed
  before same-OID retry, cleanup, or a complete tombstone.
- [x] Local worktree cleanup rejects symlinks, reparse metadata, and Unix mount/device
  crossings; retains directory identity; uses conditional quarantine for files and
  symlinks; and enforces one absolute deadline plus entry/depth/name bounds. Exactly
  owned, receipt-matched path, registration, and branch state drive cleanup, while
  ambiguous prior or quarantine-only artifacts are preserved and reported.
- [x] Failed `git worktree add` cleanup removes a registration only when exact ownership
  is proven; a forged, replaced, or otherwise unproven registration is preserved and
  reported.
- [x] Successful Git registration is attributed against a persisted pre-add namespace
  snapshot and reciprocal path/registration links. Versioned CAS ownership receipts for
  intent, generation, path, registration, branch, object ID, filesystem identities, and
  cleanup phases survive process restart for both subagent and session worktrees.
- [x] Durable ownership retains at most 64 records of at most 4 MiB each (256 MiB
  retained-record aggregate). Admission and complete-tombstone compaction use a
  persistent hard-link lock identity plus a kernel-released bounded lock; deterministic
  CAS scratch is recovered before the scan. Later record growth remains within the
  mathematical per-record aggregate bound. Collected tombstones use a deadline-bound,
  non-destructive path/ref/registration absence proof for idempotent cleanup.
- [x] Ownership CAS recovery treats canonical JSON publication as the commit point:
  target-missing evacuation restores the validated prior record, while target-present
  publication retires the validated prior artifact. Target, prior, and temporary inode
  locks prevent recovery from racing a live publisher. Receipt-bound ref-lock and
  reserved-ref scratch are recovered under bounded directory scans before phase
  reconciliation, and `Removed`/`Complete` advances only after both Git locks release.
- [ ] Windows cleanup is executed and proven through exact opened-handle deletion and
  reparse-point regressions. Local Unix cleanup already proves exact namespace
  detachment and reports rather than overclaims residual physical cleanup.
- [x] Modern abrupt-owner reconciliation remains nonterminal while cleanup ownership is
  live or proof is absent, and terminalizes only from an exact-generation FT-017 cleanup
  proof. Legacy/missing-scope records report `recovery_required`; durable background
  tasks keep their existing independent worker ownership.

### Affected Areas

`src/tools/delegation.rs`, `src/daemons/state.rs`, `src/fs_security.rs`,
`src/sandbox/mod.rs`, `src/sandbox/worktree.rs`, `src/integrations/worktree.rs`,
managed-process platform support, delegation/MCP/worktree tests, and FT-015 lifecycle
evidence.

### Validation Gates

Deterministic record replacement-before-commit and replacement-before-delete races;
held-lock deadline and anchor-only lease sweeps; hostile Git hook/filter/merge-driver and
pre-existing/replaced branch compensation tests; abrupt-owner cleanup-proof and
recovery-required tests; Windows reparse and Job Object tests; `task test`, `task check`,
`task coverage`, Windows CI `task test`, and isolated release-binary smoke.

### Static Implementation Evidence (2026-07-15)

- Record creation uses a `Missing` expectation. Mutations retain the opened record file,
  publish with `Present(&File)`, and refresh that handle after each successful transition;
  merge and verification flows carry the same revision across awaits.
- Precommit deletion compares the durable record with the attempted generation and then
  uses no-replace conditional quarantine. Process regressions cover destination
  replacement, a `Present` replacement at the precommit pause, stale revision rejection,
  and replacement immediately before conditional namespace detachment. Exact Unix
  physical unlink is not claimed.
- Record stripes and the owner namespace use persistent-anchor `try_lock` loops with an
  absolute five-second deadline. The owner namespace lives in stable `.nib`, and its
  bounded sweep reconciles the union of visible leases and persistent anchors. That
  deadline begins before setup and guards capability-relative `.nib`, lock-parent, and
  anchor-parent creation plus visible-file creation, anchor linking, durable sync,
  identity reopen, and final visibility; late expiry cannot report success, and an
  exact partial pair remains retryable. Creation and parent sync are one durability
  operation: a fresh retry that adopts an exact existing child re-syncs its parent before
  success. Records initialization derives one effective deadline and never renews it
  between namespace setup, authorization, and legacy migration.
- Compatibility with the retired per-record lock namespace is an offline migration,
  not a live dual-lock protocol. Before running
  `nib doctor --fix --confirm-no-legacy-processes`, the operator MUST stop and disable
  every prior nib binary; the explicit flag is the external quiescence attestation.
  Doctor persists a versioned pending epoch bound to the exact records-directory
  capability and exact legacy artifacts, resumes only that bounded cleanup after a
  crash, and completes the epoch only after the old namespace is clean. Ordinary
  delegation requires either that exact completed epoch or a native-origin receipt
  published atomically inside a genuinely new records directory. Existing clean but
  unmarked state, pending/rejected epochs, live or ambiguous legacy identity, and any
  legacy artifact introduced after completion all fail closed with doctor guidance and
  are never deleted by an ordinary operation.
- Native-origin staging is accepted only when its versioned receipt matches the exact
  directory identity and is its only content. Creation and no-replace publication carry
  one absolute deadline through mutation, durable parent sync, identity reopen, and
  final visibility. Doctor never recursively repairs unmarked, mismatched, or
  extra-content staging: it preserves the complete namespace for inspection and tells
  the operator to remove only that exact directory before renewed attestation.
- Modern owner loss is reconciled only after the independent supervisor persists an
  exact-generation descendant-cleanup proof. Missing or incomplete process scopes stay
  nonterminal with `recovery_required` evidence. Anchor-only live artifacts are
  preserved and reported; stale artifacts are conditionally quarantined by identity.
  Deadline-aware process-scope and session-audit locks carry that same caller-owned
  deadline through capability-relative parent creation, lock/anchor publication,
  durable sync, identity reopen, and final visibility.
- Managed Git blocks the configured executable-helper surface and initial branch
  creation carries an exact ownership receipt. Real Git still reads mutable local and
  per-worktree configuration after preflight.
- Managed-worktree intent and ownership live in versioned, atomically published CAS
  records under stable `.nib`. The initial reservation proves the final destinations
  missing, derives random staging names from its receipt ID, and records the staged
  identity before no-replace final publication. Within nib's documented isolation model,
  the unguessable reserved name is the pre-CAS creation capability; a malicious same-UID
  peer that reads and substitutes reservation state remains outside the boundary. Restart
  rehydration reopens the persisted path, registration namespace, registration, common
  Git directory, and branch anchor identities; identity-distinct replacements are
  preserved. Explicit branch adoption requires that generational record and advances its
  object ID, ref-file identity, and anchor through a recoverable CAS transition.
- Ownership CAS restart recovery validates the key and bytes of canonical/prior records
  and probes kernel ownership before resolving an interrupted evacuation. Canonical
  absence rolls back; canonical presence finalizes the committed generation. Initial
  branch staging is identity-CAS-persisted before receipt-marked packed/target lock
  acquisition. Matching dead locks and legacy deletion quarantines are removed by exact
  identity; live or unrecognized artifacts remain nonterminal. A valid foreign packed
  lock is deferred until its owning receipt is visited, avoiding record-order-dependent
  compaction failure.
- Cleanup writes `removing` before each exact deletion and `removed` afterward. Restart
  reconciliation resumes from those phases, promotes a completed pre-finalization Git
  transaction when reciprocal evidence matches, compensates an incomplete intent from
  exact persisted provenance, and retains a durable complete tombstone. Failed-add
  registrations that cannot be attributed remain preserved.
- Complete tombstones are deterministically compacted only under a stable persistent
  lock domain. The retained-record namespace is bounded to 64 records and 256 MiB;
  deterministic atomic-transaction scratch has a separate worst-case bound of two
  additional 4 MiB files per record (768 MiB total including retained records) and is
  strictly recovered before compaction. Quarantine-only branch deletion remains
  non-Complete and preserves its retained anchor for later exact physical recovery.

Focused Linux validation after the final publication-receipt migration passed:
`cargo test registration_failure_ --lib -- --test-threads=1` (2 tests),
`cargo test post_publication_ --lib -- --test-threads=1` (3 tests),
`cargo test anchor_only --lib -- --test-threads=1` (3 tests), and
`cargo test precommit_cleanup_preserves_ --lib -- --test-threads=1` (3 tests).
These cover exact and non-exact registration-failure receipts, post-publication
substitution before handle refresh, identity-distinct JSON-equivalent cleanup races,
and live/dead anchor-only owner reconciliation through status/list/cancel paths.
Additional regressions cover
`managed_git_rejects_executable_repository_helpers_without_running_them`,
`managed_git_rejects_executable_worktree_configuration_without_running_it`,
`failed_add_preserves_a_registration_forged_after_the_snapshot`,
`reported_add_failure_preserves_unproven_registration`,
`normal_remove_uses_exact_ownership_to_remove_path_registration_and_branch`, and
`cleanup_retry_resumes_after_branch_deletion_and_lock_release_failure`.
The offline-lock migration matrix additionally covers a prior-version child paused
after opening the exact legacy anchor and before acquiring its lock: ordinary status or
record work preserves the inode and refuses to enter the fixed-stripe critical section;
only after the child terminates may an explicit attested doctor run reconcile it. A
fresh legacy artifact after epoch completion again blocks ordinary work until a fresh
attestation.

Focused durable-ownership validation on 2026-07-16 passed 53/53 sandbox worktree tests
and 11/11 integration worktree tests. The restart matrix covers process-local receipt
loss, session-manager rehydration, incomplete creation intent, write-ahead cleanup
reconciliation, adopted branch revisions, same-OID ref replacement, missing
generational receipts, path replacement preservation, pre/post staging-CAS crashes,
anchor-only removal restart, quarantine-only reporting, killed compaction holders, stale
CAS recovery (target-missing rollback and target-present finalization), prior-anchor
retirement, killed receipt-marked packed/target locks, reserved-ref atomic scratch,
legacy ref-lock quarantine recovery, foreign-owner deferral, and collected-tombstone
idempotency.
Packed-ref ambiguity is checked under both Git lock domains before loose-ref or anchor
removal, including Removing-phase restart. Partial-add restart remains nonterminal when
bounded comparison with the pre-add registration snapshot finds an unattributed admin
entry.

### Local Validation Evidence (2026-07-16)

- [x] `task check`, `task test`, `task docs:check`, and the locked release build pass.
- [x] The repository executes 772 tests: 588 library, 53 CLI, and 131 integration tests.
- [x] `task coverage` passes at 83.94 percent line coverage (53,734/64,015).
- [x] The optimized Linux release binary passes noninteractive MCP, durable-task,
  scheduled-wake, doctor, skill, context, and raw-PTY approval/question/cancellation
  smoke tests.
- [x] Delegation integration passes 21/21, including owner loss, cancellation,
  verification, recovery, exact cleanup, and fail-closed ambiguity paths.
- [ ] Windows runtime reparse, handle-deletion, process-containment, and full-suite gates
  remain unexecuted because a Windows runtime is unavailable on this host. macOS
  runtime execution also remains unverified.

### Explicit Future Boundary

FT-015 delegates abrupt-owner descendant containment to
[FT-017](ft_017_managed_process_supervisor.md). The local Linux implementation uses an
out-of-process supervisor, durable cleanup lease, and bwrap PID namespace and proves a
real `setsid` descendant is reaped before terminal publication. Production delegation
fails closed on Windows and macOS before creating worktree or ownership state. Their
Job Object and group-contained implementations remain native mechanism tests until
cleanup authority is isolated from managed workers; macOS never claims arbitrary
detached-descendant containment.

Runtime destruction now owns its supervisor control guard before the monitor future is
submitted, so even an unpolled future sends explicit cancellation and reaches a durable
`cancelled` record only after cleanup proof. Ten consecutive runtime-drop regressions
passed. A locally tracked legacy task is still aborted synchronously when its durable
record cannot be reconciled; the operation remains explicitly unresolved, the legacy
record stays `running`, and no cancellation success is reported without durable proof.
Production containment preflight now probes only the managed-process backend. It does
not run the unrelated Git availability diagnostic before entering cancellable worktree
creation; the portable MCP regression that stalls every Git invocation passes 10/10 and
returns a reconciled cancellation response without publishing subagent state.

## Hosted Windows Git Fixture Follow-up (2026-07-21)

### Scope

Make Git-backed integration repositories deterministic when the host Git installation
enables line-ending conversion through system or global configuration. Recovery
fixtures that manually establish an already-integrated commit and executor/end-to-end
fixtures that inspect patched worktree bytes must use the same repository-local
conversion policy seen by nib's managed Git, which deliberately disables ambient
configuration sources. Assertions over persisted verification evidence must compare the
canonical project root required by delegation rather than the runner's potentially
DOS-short spelling of the same temporary directory.

### Acceptance Criteria

- [x] Git-backed delegation, executor, and end-to-end integration repositories pin
  `core.autocrlf=false` before their first commit, so fixture setup and managed Git
  interpret worktree bytes consistently.
- [x] Already-integrated recovery records verification against the canonical parent
  worktree even when the test runner supplies an equivalent DOS-short project path.
- [x] Already-integrated merge recovery remains clean and succeeds on Windows when the
  host Git installation enables `core.autocrlf` outside the repository.
- [x] Worktree patch assertions remain byte-stable when Windows enables ambient
  line-ending conversion.
- [x] The exact implementation revision passes the hosted Windows integration suites
  and full CI matrix without weakening managed Git's ambient-configuration isolation.

### Affected Areas

`tests/delegation.rs`, `tests/executor.rs`, `tests/test_runtime_e2e.rs`, FT-015 validation
evidence, and the hosted CI matrix.

### Validation Gates

`task test`, `task check`, Windows-target `task check:all-targets`, and the exact-revision
hosted Linux, macOS, and Windows jobs.

### Local Validation Evidence

With an isolated global Git configuration setting `core.autocrlf=true`, `task test`
passes 617 library tests, 62 binary tests, and every integration and documentation suite.
This includes 21 delegation tests, 15 executor tests, and 9 end-to-end runtime tests.
The canonical `task check` gate passes, and the all-target/all-feature graph cross-checks
for `x86_64-pc-windows-msvc`. Hosted Windows run `29790855194` reached the repaired
already-integrated recovery path and persisted its canonical parent worktree, then exposed
only a lexical assertion against the runner's equivalent DOS-short temporary path. The
assertion now derives the expected canonical root. A fresh hostile-config `task test`,
canonical `task check`, documentation integrity gate, and Windows all-target cross-check
pass; an independent path-assertion audit found no related mixed-representation equality.

### Final Hosted Fixture Evidence

Hosted run `29859138441` on exact implementation revision
`769f67b200af70531129f7578cead29862d24c8c` passed Validate, macOS Tests, and Windows
Tests. Windows passed `integrated_pending_commit_ignores_stale_worktree_for_verification_then_cleans_it`,
`pending_merge_recovers_after_commit_and_cleanup_preceded_final_write`,
`approved_patch_physically_changes_only_the_session_worktree_and_is_verified`, and
`full_agent_loop_compresses_edits_and_runs_real_cargo_tests_in_one_worktree`, plus the
complete 548-test library suite and hosted matrix. This closes the Git-fixture follow-up
without weakening managed Git's ambient-configuration isolation.

## Hosted Windows Bounded Cleanup and Merge Contention Follow-up (2026-07-21)

### Scope

Make all non-cancellable synchronous worktree-creation compensation routed through
`compensate_failed_create_sync` use the normal bounded Git cleanup window instead of
the shorter cancellation-response window. This applies the same bounded budget to its
pre-add and post-add helper calls, while cancellable creation cleanup remains
independently bounded at three seconds. Make legitimate
repository-wide contention between two different subagent merge workflows wait through
a healthy Windows Git critical section without weakening the persistent lock identity,
replacement checks, cancellation behavior, or explicit short-timeout failure path.
Thread the executor cancellation signal through verification preparation and final
merge-lock acquisition so the longer healthy-contention budget does not delay a
cancelled request or mutate its authoritative subagent record.
Improve the post-add regression so any returned composite cleanup error is surfaced
before the residual-artifact assertions and included in their failure diagnostics.

### Acceptance Criteria

- [x] Every non-cancellable synchronous creation-failure call routed through
  `compensate_failed_create_sync` uses the normal 30-second Git cleanup budget to
  remove the provably exact-owned path, reciprocal registration, and owned branch
  available at that phase.
- [x] Cancellable worktree creation retains its independent three-second cleanup bound.
- [x] `sync_create_compensates_a_failure_after_worktree_add` surfaces any returned
  composite compensation error before artifact assertions, includes the creation error
  in each residual-artifact diagnostic, and requires successful compensation to leave no
  visible path, registration, or branch.
- [x] The default repository merge-lock wait is a bounded 30 seconds, allowing a healthy
  Windows contender to span the other workflow's preparation or integration critical
  section without entering the repository concurrently.
- [x] Explicit short-timeout repository-lock tests still prove deterministic timeout,
  persistent-anchor identity, and replacement-domain rejection.
- [x] A merge cancelled while waiting behind a held repository lock returns promptly,
  releases its acquisition attempt, and leaves the authoritative subagent record
  unchanged.
- [x] The cross-ID merge regression remains deadlock-bounded and requires both merge
  records and artifacts to reach their successful terminal state.
- [x] Focused regressions pass repeatedly and the exact implementation revision passes
  the hosted Linux, macOS, and Windows matrix.

### Affected Areas

`src/sandbox/worktree.rs`, `src/tools/delegation.rs`, `src/tools/executor.rs`,
`tests/delegation.rs`, focused worktree and repository-lock tests, this FT-015 evidence,
and the hosted CI matrix.

### Validation Gates

Focused post-add compensation, repository-lock timeout/identity/cancellation, and
cross-ID merge tests; `task fix`, `task test`, `task check`, `task docs:check`, Windows-target
`task check:all-targets`, `git diff --check`, two-stage review, and the exact-revision
hosted matrix.

### Local Validation Evidence

`SYNC_CREATE_CLEANUP_TIMEOUT` now follows the existing 30-second Git command budget for
every non-cancellable synchronous creation-failure call through
`compensate_failed_create_sync`, while
`CANCELLED_CREATE_CLEANUP_TIMEOUT` remains three seconds. The post-add regression
rejects and prints any returned composite cleanup failure before checking path,
registration, and branch absence, and every artifact assertion carries the original
creation error. Repository merge-lock acquisition remains an absolute polling deadline,
but its production contention budget is 30 seconds; a direct contract test fixes that
default while the existing injected 75-millisecond and one-second timeout tests remain
unchanged. The held-lock cancellation regression proves prompt failure, byte-identical
authoritative state, and successful reuse by a fresh uncancelled merge. The cross-ID
integration test also has a 90-second outer deadlock bound while still requiring both
real merges and both durable records to reach `merged`.

The affected post-add, repository-lock timeout/identity, persistent-anchor,
lock-cancellation, and cross-ID merge tests passed in two serialized final-patch
executions across `task test` and `task check`. Each execution passed all 620 library
tests, 62 CLI tests, 22 delegation tests, every integration suite, and all nine runtime
E2Es. An earlier pre-contract-test execution continued past every then-affected test and
hit the pre-existing intermittent Linux namespace-recovery timing assertion; its
immediate canonical rerun was green. `task fix`, `task check`, `task docs:check`,
Windows-target `task check:all-targets`, and `git diff --check` pass.

### Reproduction Evidence

Hosted run `29851652973` on revision
`8eaf77a1ab2d82183b3f6629f7ccd0744d8be516` passed Validate and macOS. Windows attempt
one, job `88705766329`, passed 546 of 547 library tests, then
`sync_create_compensates_a_failure_after_worktree_add` reported that its partial
worktree path remained. The test took 12.70 seconds where the same source had passed in
roughly two seconds on each of the five preceding Windows runs; its assertion omitted
the returned cleanup error, so the log could not distinguish deadline exhaustion from a
transient sharing violation.

Windows attempt two, job `88710321275`, passed all 547 library tests, proving that the
first failure was intermittent. It then passed 13 of 14 delegation integration tests;
`merges_for_different_subagent_ids_share_one_repository_lock` failed because one healthy
contender exhausted the default five-second repository-lock wait while the other merge
workflow held the shared persistent lock. The test releases its independent exclusion
fixture after 100 milliseconds, and both workflows acquire the same lock during
verification preparation and final integration. The five-second budget therefore
measured legitimate serialized Git work rather than a stale holder. Neither failure was
caused by the workflow-only smoke change from `7ff72c857739be273531bd914ba5f50c66c82670`
to `8eaf77a1ab2d82183b3f6629f7ccd0744d8be516`.

### Final Hosted Remediation Evidence

Hosted run `29859138441` on exact implementation revision
`769f67b200af70531129f7578cead29862d24c8c` passed Validate, macOS Tests, and Windows
Tests. Windows job `88731054375` passed all 548 library tests, including
`sync_create_compensates_a_failure_after_worktree_add`; all 15 delegation tests,
including the held-lock cancellation/reuse regression and the cross-ID serialized merge
regression; all nine runtime E2Es; the release build; and the release-binary `--help`,
`version`, and `doctor` smoke. The exact hosted run therefore closes this bounded-cleanup
and contention follow-up while FT-015 retains its separate platform-authority work.

## Remaining Implementation Plan

1. Execute Windows/macOS runtime gates for worktree identity/deletion and the FT-017
   native mechanisms, and prove that production delegation continues to fail closed on
   both platforms. Enabling production delegation outside Linux requires the separate
   FT-020 authority design and is not a completion condition for this v1 contract.
2. Rerun the canonical Task gates and two-stage review before moving FT-015 to `done/`.

## Non-Linux Production Scope Decision (2026-09-02)

FT-015 defines production delegation as Linux plus a usable bwrap PID namespace. Native
Windows Job Object and macOS process-group implementations remain qualification
mechanisms, and their runtime tests plus explicit production rejection are required
before this spec can close. They do not authorize production delegation on those
platforms.

An OS-protected cleanup authority that remains effective after owner loss and is
inaccessible to the managed worker is a separate product capability. FT-020 owns that
future design and rollout. FT-015 must not remain indefinitely open waiting for FT-020,
and FT-020 must not weaken this spec's current fail-closed behavior.

## Windows Rollback-Fixture Positive-Progress Budget (2026-09-02)

### Scope

Keep the synchronous/cancellable initial-record failure regression focused on exact
fallback-audit rollback rather than the unrelated default preparation deadline. The
production five-second preparation timeout and dedicated expiry/deadline regressions
remain unchanged.

### Acceptance Criteria

- [x] The rollback fixture uses an explicit 30-second test-only positive-progress budget
  and reports the unexpected bounded error if injected record publication is not reached.
- [x] The test-only timeout guard cannot move across threads, and a repository contract
  pins the focused Task selector to the fixture's unique Rust test name.
- [ ] The exact PR revision passes the full hosted Windows job and native matrix after
  this stabilization.

### Affected Areas

`src/tools/delegation.rs`, `tests/installers.rs`, `Taskfile.yml`, this spec, and
exact-revision hosted Windows validation.

### Validation Evidence

Hosted run `33632000483` passed the Windows pseudoterminal and all-target gates, then
reached 973 passing library tests before this rollback fixture failed after approximately
15 seconds because the expected injected publication error was not reached. The test's
purpose is rollback equivalence between synchronous and cancellable entrypoints; separate
regressions retain the production deadline contract. `task test:delegation` now selects
this exact unit fixture in addition to the state, managed-process, and integration paths.
The focused task passed both optional-open race tests, this exact rollback fixture, all
36 managed-process tests, and all 22 delegation integrations. The replacement hosted
matrix remains open.

## Crash-Durable Spawn Preparation Follow-up (2026-08-29)

Every delegated spawn is covered by a versioned write-ahead spawn-preparation intent under
the authoritative subagent records namespace. The intent is published before worktree,
child-config, owner, or shared profile/session mutation and binds the subagent id, execution
generation, owner lease, exact durable worktree authority, and planned audit
session/destination. Fallback audit initialization adds its exact namespace/session receipt;
a provided store instead binds the exact preexisting audit target without claiming its
namespace. Publishing a `running` record does not supersede the intent: until execution
handoff is durable, that record carries private `pending` handoff evidence and remains a
compensable preparation artifact. The intent is never projected as a public subagent or
result, and the private handoff evidence is stripped from every public response.

Spawn, list, and status entry points reconcile unfinished intents with a bounded,
fail-closed protocol. A live owner preserves the intent. A dead preparation without a
running record removes only its exact audit leaf and empty transaction-owned ancestors,
then its exact worktree and owner pair, and deletes the intent last. A running record
retains all workload resources and permits only the stale intent to be removed. Every
step is retry-idempotent; missing, half-cleaned, mismatched, nonempty, or replacement
state is preserved rather than guessed.

Acceptance and validation include a subprocess killed after audit-session publication
but before running-record publication, restart reconciliation with no per-spawn audit,
worktree, owner, or intent orphan, a hidden preparing state, and a successful fresh retry.

The initial intent contains durable ownership evidence before every earlier audit
namespace mutation: an exact retained-ancestor identity, post-worktree proven-missing
component chain, retained identity/anchor state, and domain-separated transaction marker
bytes. The session identity marker is written and synced with those bytes before its
anchor is linked. The intent is upgraded before the session leaf with the exact planned
session payload and directory authorities, then upgraded again with the published leaf
identity. A crash between those upgrades is recoverable only when the stable leaf bytes
exactly match the durable plan; mismatches remain fail closed.

Acceptance includes subprocess termination at directory-create, marker, anchor, sync,
final-visibility, session-publication, and pre-record boundaries. Restart must reconcile
each phase idempotently, while concurrent adopted sessions and hostile or ambiguous
namespace entries remain byte-identical.

The initial revision now precedes every worktree/config/owner mutation and binds exact
pre-generated authorities that the resource constructors must consume. Both synchronous
and cancellable spawn use one retained, Policy-B-authorized records-directory capability
for preparation recovery, intent access, and canonical record supersession checks. A
running or terminal record may retire the intent only after exact status and authority
validation, including the worktree durable receipt and pinned audit destination. Atomic
publication errors retain and finalize exact receipts; indeterminate transaction artifacts
or cleanup errors remain visible and fail closed for deterministic restart recovery.

The spawn writer acquires the durable global migration fence and its exact fixed record
stripe before publishing `planned`, in that order, and retains both authorities through
every resource mutation, intent revision, running-record publication, compensation, and
intent retirement. Preparation reconciliation acquires the same global fence before
atomic recovery, intent reads, external cleanup, record supersession, or deletion. It can
therefore neither roll back an evacuated revision nor retire a quarantined intent while a
live writer owns the transaction; it remains bounded and creates no per-intent lock file.
All owner, audit-session, and worktree cleanup additionally receives the retained records
capability as a pre/post mutation guard, so a detached or replaced records namespace
preserves the intent and its external resources fail closed.

Restart recovery resolves the exact planned session leaf transaction before classifying
the canonical leaf. A canonical plus temporary entry is accepted only when both are the
same unlocked publication identity with the exact planned bytes; prior, mismatched, live,
or ambiguous artifacts are preserved. The intent remains authoritative until session
temporary/previous/deletion-quarantine artifacts and transaction-owned marker, anchor,
and empty ancestor directories have all been finalized.

Ordinary spawn compensation uses the preparation authority's original absolute operation
deadline; no worktree, owner, audit-session, or intent cleanup receives a renewed budget.
Intent retirement is cleanup-last: a partial or indeterminate worktree constructor result,
any exact-resource cleanup error, post-audit cancellation cleanup error, or expiry after a
durable resource mutation leaves the intent authoritative for bounded restart recovery.
Only after every exact external cleanup succeeds may the intent be removed. Deterministic
sync/cancellable regressions cover durable worktree reservation, fully written session
temporary/canonical publication, and injected session, owner, worktree, and audit cleanup
failures, then prove a fresh restart removes the resources before retiring the intent.

Execution admission uses a durable handoff protocol. The prepared record is published with
`pending` evidence, TaskManager registration is paused behind a one-shot start gate, and
the intent records `record_published` and `manager_registered`. Only after the worker or
native supervisor has established its abort/control owner does the intent advance to
`handoff_proven`; an exact record compare-and-swap then changes the matching evidence to
`committed`. For native execution, the version-4 intent preplans an immutable cleanup-lease
id and supervisor-registration nonce, and the parent publishes their exact `Prepared`
scope with `launch_committed=false` before spawning the supervisor. The spawned
supervisor's first workload-state operation—before reading the parent request, record,
worktree, or any other external launch resource—is to capture its own OS identity and
exact-CAS that identity into the matching Prepared scope under the fixed process-scope
lock. It then spawns the worker behind the real OS launch gate, persists a `Running`
process scope with `launch_committed=false`, and sends
a bounded, versioned `READY` frame. The frame and subsequent `COMMIT`/`STARTED` frames bind
one fresh nonce plus the exact subagent id, execution generation, owner lease, cleanup
lease, supervisor identity, and direct-child identity. The parent only observes and
validates the already-persisted exact supervisor identity; it is not a second publisher.
The version-4 preparation intent
persists that complete READY process-scope authority before the parent sends `COMMIT`; the
intent and record commits must both be durable. The supervisor exact-validates it, durably
advances the same scope to `launch_committed=true`, releases the OS gate, and responds
`STARTED`; only then may the parent release TaskManager and retire the intent. Partial or
malformed frames, identity mismatch, timeout, EOF, or parent death before `COMMIT` abort
the gated child, prove descendant cleanup, and persist launch-abort authority without ever
running the worker. Final intent retirement is cleanup-last and still uses the original
absolute spawn deadline.

That startup deadline governs only existing-only process-scope opening,
self-registration, and the `READY`/`COMMIT`/`STARTED` launch handoff. Once `STARTED` is
successfully acknowledged, the supervisor converts the retained exact store capability
and already-held cleanup lease into capability-identical long-lived authorities; it does
not reopen the scope namespace by path. Later cleanup and terminal mutations each use a
fresh bounded process-scope lock deadline, allowing normal completion or cancellation
after an arbitrarily long worker lifetime while retaining exact cleanup proof. No deadline
renewal occurs on the pre-commit path.

The version-4 process-scope plan is immutable across every intent revision and atomic
previous/target successor. Structural validation, transaction recovery, committed-evidence
evaluation, and restart classification all apply the same strict binding: the READY cleanup
lease must equal the preplanned lease and its supervisor-registration nonce must be present
and exactly equal to the preplanned nonce. Scope id, subagent kind, generation, owner,
backend, supervisor, and direct-child identity remain exact between READY and every observed
successor. A changed lease or nonce, missing nonce, or substituted atomic revision preserves
the intent and all external resources byte-for-byte and fails closed. Legacy version-2 and
version-3 intents never acquire an inferred version-4 plan.

Preparation compatibility is an explicit version/field matrix rather than a permissive
serde default. Version 2 never carries either a process-scope plan or a persisted `READY`
scope, including at `HandoffProven`, and therefore can never be promoted from otherwise
matching committed process state. Version 3 carries no version-4 plan and may add its exact
legacy `READY` representation only on the monotonic `ManagerRegistered` to
`HandoffProven` transition; later evidence must match every persisted identity exactly.
Version 4 follows the stricter plan-bound rules above. Structural reads, revision recovery,
temporary/previous transaction inspection, evidence evaluation, and restart classification
apply the same matrix. Forbidden legacy fields are preserved byte-for-byte and fail closed;
they are neither inferred away nor adopted from an atomic artifact.

Restart opens the process-scope namespace only through the retained, Policy-B-authorized
records capability and one deadline-bound store lock. That locked lookup recovers and scans
the canonical record, deterministic atomic temporary/previous artifacts, scope-deletion
quarantine, cleanup lease, and cleanup-lease quarantine; only one coherent empty scan may
report absence. A committed record is authoritative only when the stored version-4 READY
scope exactly matches its key, `subagent` kind, generation, cleanup authority, backend,
owner, supervisor, and child identities and the observed scope is
`launch_committed=true` in `Running`, `CleanupInProgress`, or `RecoveryRequired`, or is
`Complete` with the exact terminal cleanup proof carried by the record. `Prepared`, false,
legacy missing launch flags, mismatched keys/kinds/authority, TaskManager presence, and
unmatched terminal proof never supersede the intent. A process scope created during any
pre-handoff intent phase is cleanup authority: restart must abort/reap its exact descendants
and retire its exact scope and cleanup lease before removing owner, audit session, worktree,
record, or intent. A pending record, an uncommitted record, or a committed record without
committed execution evidence is rolled back under a fresh bounded recovery authority.
An exact version-4 Prepared scope with no supervisor, direct child, or cleanup lease is
retired under the same scope lock used by self-registration. This is safe against a late
unobserved supervisor: either its CAS wins first and restart observes and aborts that exact
identity, or retirement wins and the late CAS fails before the supervisor can access any
external resource. The lock ordering is identical on Unix and Windows.
Ambiguous evidence remains fail closed. Deterministic
sync/cancellable expiry tests cover final retirement and rollback failure; subprocess tests
cover each durable handoff boundary, SIGKILL at both scope and cleanup-lease quarantine
boundaries, real-binary parent SIGKILL after Prepared-before-spawn and after OS spawn-before
self-registration, plus parent SIGKILL after `READY` and before `COMMIT`, with a
worker-entry sentinel proving zero precommit execution. No restart may expose an unlaunchable
`running` or `recovery_required` workload.

## Current Risks

- Ambiguous registrations and cleanup races must remain preserved for operator review;
  guessing ownership would be destructive.
- Residual physical cleanup is reported when exact namespace ownership cannot be carried
  through unlink; hostile same-UID peers require an external isolation boundary.
- A real Windows repository whose checkout semantics exist only in ambient Git
  configuration can appear dirty to isolated managed Git. Supporting that case while
  retaining FT-015 configuration isolation requires a separate policy design.
- Windows and macOS runtime behavior remains unexecuted on the local Linux host.
