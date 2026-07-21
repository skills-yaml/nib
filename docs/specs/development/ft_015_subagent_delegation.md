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
  bounded sweep reconciles the union of visible leases and persistent anchors.
- Modern owner loss is reconciled only after the independent supervisor persists an
  exact-generation descendant-cleanup proof. Missing or incomplete process scopes stay
  nonterminal with `recovery_required` evidence. Anchor-only live artifacts are
  preserved and reported; stale artifacts are conditionally quarantined by identity.
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
configuration sources.

### Acceptance Criteria

- [x] Git-backed delegation, executor, and end-to-end integration repositories pin
  `core.autocrlf=false` before their first commit, so fixture setup and managed Git
  interpret worktree bytes consistently.
- [ ] Already-integrated merge recovery remains clean and succeeds on Windows when the
  host Git installation enables `core.autocrlf` outside the repository.
- [ ] Worktree patch assertions remain byte-stable when Windows enables ambient
  line-ending conversion.
- [ ] The exact PR revision passes the hosted Windows integration suites and full CI
  matrix without weakening managed Git's ambient-configuration isolation.

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
for `x86_64-pc-windows-msvc`.

## Remaining Implementation Plan

1. Execute Windows/macOS runtime gates for worktree identity/deletion and the FT-017
   native mechanisms, then design an authority boundary inaccessible to managed workers
   before enabling production delegation on either platform.
2. Rerun the canonical Task gates and two-stage review before moving FT-015 to `done/`.

## Current Risks

- Ambiguous registrations and cleanup races must remain preserved for operator review;
  guessing ownership would be destructive.
- Residual physical cleanup is reported when exact namespace ownership cannot be carried
  through unlink; hostile same-UID peers require an external isolation boundary.
- A real Windows repository whose checkout semantics exist only in ambient Git
  configuration can appear dirty to isolated managed Git. Supporting that case while
  retaining FT-015 configuration isolation requires a separate policy design.
- Windows and macOS runtime behavior remains unexecuted on the local Linux host.
