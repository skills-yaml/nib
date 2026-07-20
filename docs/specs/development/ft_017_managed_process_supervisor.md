# FT-017: Managed Process Supervisor for Abrupt Owner Loss

**Status:** Development

## Summary

Run foreground subagent execution inside an independently supervised process scope so
an abrupt nib owner exit cannot leave Git, terminal, MCP, or agent descendants alive
while the workload record reports cleanup or terminal reconciliation as complete.

## Problem

The current Unix runtime places managed children in process groups and kills those
groups from normal wait, cancellation, and `Drop` paths. `SIGKILL`, power loss, and
equivalent whole-process failure execute none of those paths. Process-group ownership
does not make the kernel terminate the group, and a descendant can escape with
`setsid`. The current owner lease therefore proves that the Rust owner process died,
but it does not prove that all foreground descendants were reaped.

Windows Job Objects already provide a kernel kill-on-close primitive. Linux can provide
strong containment with a PID namespace or delegated cgroup. macOS has no equivalent
public primitive for arbitrary descendants that detach from their process group, so its
supported contract must be explicit rather than overstated.

## Goals

- Separate subagent workload ownership from cleanup ownership so reconciliation cannot
  outrun descendant termination.
- Make abrupt owner loss observable through an unforgeable EOF/handle signal.
- Persist a bounded execution generation and cleanup lease that fence late owners and
  terminal publication.
- Provide strong Linux production containment and retain truthful, non-production
  Windows/macOS backend evidence until their durable authority boundary is protected.
- Cover foreground Git, terminal, MCP, skill, and agent-loop processes through one
  registered scope root.
- Preserve the independent lifecycle of durable background task workers.

## Non-Goals

- Containing processes that nib did not start.
- Treating operator-launched daemonization as a supported foreground tool behavior.
- Replacing durable task-worker ownership for explicitly background terminal jobs.
- Claiming arbitrary detached-descendant containment on macOS without a new kernel or
  service-manager mechanism that can prove it.

## Design

### Worker And Supervisor

Launch each subagent as a hidden worker OS process rather than only a Tokio task in the
interactive owner. An independent supervisor owns the execution scope and a cleanup
lease. The interactive owner holds the write end of a close-on-exec pipe; EOF tells the
supervisor that the owner exited even when no Rust destructor ran.

The outer launcher records the exact hidden-supervisor identity before writing any
request byte. The supervisor then records the subagent ID, execution generation,
cleanup-lease ID, and platform scope identity before starting foreground work. On Linux
bubblewrap executes a fixed PID-1 launch gate through an inherited `--sync-fd` socket.
The gate reports readiness only after namespace setup, exits without executing the
workload when the supervisor endpoint closes, and accepts the launch frame only after
the validated namespace identity is durable. The active supervisor is the only
component allowed to mark normal cleanup complete. After it crashes, an exact recovery
path may publish cleanup or launch-abort authority under the same generation and lease
fences. Normal completion, cancellation, and owner loss terminate, reap, verify, and
audit the scope before releasing the cleanup lease.

### Reconciliation

A running record with a dead owner but a live cleanup lease remains nonterminal. A fresh
process may report `recovery_required` or `cleanup_in_progress`, but cannot report that
the descendant tree was reaped. Once the active supervisor or exact recovery owner
durably records authority and releases any cleanup lease, generation-fenced
reconciliation publishes the final interrupted or failed state exactly once.

On Linux, a crashed supervisor may be recovered only while holding the exact cleanup
lease. When the lease file was never created, recovery may create and lock it only after
proving the exact supervisor is absent and the authoritative record is still Prepared
with no namespace root. Because supervisor lease acquisition precedes bwrap spawn, that
state proves the workload was never created. The supervisor obtains the namespace-root
host PID from bwrap's bounded `--info-fd` report, validates that it is the outer
monitor's child and PID 1 in the nested namespace, waits for the fixed PID-1 gate's
readiness frame, and persists that exact start-marker identity as `direct_child`.
Prepared state retains the exact supervisor identity even before bwrap starts. If the
supervisor dies before Running, the durable Prepared phase proves that no launch frame
was published; supervisor EOF
closes the fixed gate. Recovery publishes a generation- and lease-bound launch-abort
proof with `workload_never_launched=true`, plus the namespace-root identity when one was
durable. It explicitly does not claim `cleanup_verified` or descendant cleanup. When a
namespace root was recorded, recovery exact-signals it through a pidfd and waits for its
absence before publishing the proof. If the supervisor dies after Running, normal and
recovery cleanup signal the recorded identity through a pidfd only after recapturing the
same start marker. Recovery completes only after both recorded identities are absent,
then publishes the normal cleanup proof before releasing the lease. Mismatched
identities, stale generations, ambiguous non-Prepared state, and unsupported backends
remain nonterminal.

### Platform Backends

- Linux: use a PID-namespace init owned by the supervisor through bwrap. Bounded
  `--info-fd` discovery plus an `--as-pid-1` command gate carried by `--sync-fd` binds
  launch to the validated namespace PID 1 without pausing bubblewrap in its pre-init
  parent-death window. EOF aborts before user code; after launch, death of PID 1
  terminates every process in the namespace, including descendants that call `setsid`.
  Production eligibility runs this exact info/socket/pidfd-kill protocol once and caches
  the fail-closed result. A delegated cgroup-v2 scope with `cgroup.kill` is an acceptable
  alternative when its ownership can be proven.
- Windows: attach the suspended worker to a kill-on-close Job Object before execution,
  retain the handle in the supervisor, and rely on normal Job inheritance for its
  descendants (including launchers that also create a nested managed Job). This remains
  backend-only evidence: production delegation is rejected before creating state because
  the managed worker can currently reach pathname-based cleanup authority.
- macOS: support a documented group-contained foreground contract with a supervisor
  watching owner EOF and killing/verifying the dedicated process group. Reject or report
  unsupported strong containment when a workflow requires arbitrary daemonization;
  do not claim that `killpg` reaches a descendant that escaped with `setsid`. Production
  delegation is rejected because the worker can reach cleanup state and a crashed
  supervisor has no independent cleanup owner.

### Launch And Durable State Fencing

The parent persists the exact supervisor generation and establishes readiness and exit
monitors before writing the first request byte. Before delivery, a bounded supervisor
reap permits exact removal of Prepared state. Once any byte is written, every error
closes or cancels the control channel and leaves the owner lease with the exit monitor;
no direct terminal publication or scope deletion is allowed without cleanup or
launch-abort authority.

Process-scope storage uses one fixed durable store lock, bounded
entry/name/record/aggregate-byte limits that reserve the full temporary/previous/target
publication peak, strict recovery of deterministic atomic scratch,
generation-validated monotonic scope transitions, and proof-bound deletion quarantine
recovery. A Complete scope retires only under the subagent record stripe lock after a
full authoritative `SubagentRecord` snapshot supplies the exact execution generation
and either its cleanup proof or distinct launch-abort proof, and only after its cleanup
lease is absent. Retirement authority remains durable through later verification and
merge statuses so transient retirement failures can be retried. Schema version 2 binds
Linux `direct_child` to namespace PID 1; version 1 state is preserved and rejected for
that scope without blocking unrelated version-2 generations.

### Registered Scope Root And Child Inheritance

Register the platform scope root once before the owner receives `READY`, then launch the
hidden subagent worker beneath it. Terminal execution, raw patch Git, async and
synchronous managed Git, outbound MCP, skill hooks, and agent-loop provider children
inherit that worker's PID namespace or Job Object. This avoids a proof gap between
arbitrary per-child registration calls while still letting each existing launcher retain
its bounded normal completion and cancellation behavior.

On macOS, where containment is a process-group contract rather than a kernel process
tree, managed child launchers suppress creation of an inner process group while the
scope marker is present. Raw child launches inherit the worker group normally. A child
that deliberately calls `setsid` remains outside the documented macOS guarantee. Unix
launchers retain numeric process-group authority only when they created the group and
use `waitid(..., WNOWAIT)` to pin the leader identity while signalling lingering group
members before the final reap.

## Affected Areas

`src/main.rs`, `src/tools/delegation.rs`, `src/agent/loop.rs`,
`src/tools/executor.rs`, `src/tools/core.rs`, `src/sandbox/mod.rs`,
`src/sandbox/worktree.rs`, `src/sandbox/windows_job.rs`,
`src/integrations/mcp.rs`, `src/skill_cmd.rs`, `src/sandbox/process.rs`, workload/session
audit records, and cross-process tests.

## Alternatives Considered

- Process groups alone: retained for graceful cleanup but rejected for abrupt-owner
  guarantees because the kernel does not kill a group when its owner dies.
- `PR_SET_PDEATHSIG` on direct children: insufficient because it is not inherited across
  fork and does not contain detached grandchildren.
- Reconciliation-time process scans: rejected as the primary proof because PID reuse,
  reparenting, and `setsid` make ancestry scans racy.
- Require bwrap on every Unix platform: rejected because macOS cannot provide it and the
  current direct execution contract would regress without an explicit product decision.

## Rollout

The durable cleanup-scope record, hidden worker/supervisor protocol, and Linux
production backend are implemented. Foreground subagents fail closed before creating
worktree or ownership state unless Linux passes the exact bwrap gate and pidfd cleanup
probe. The worker is the single registered scope root: its
terminal, MCP, raw Git, managed Git, and agent-loop descendants inherit that kernel
scope. Windows and macOS retain native backend tests but are not production-enabled.
Durable background jobs, schedules, and nested subagents are rejected from that
foreground scope and keep their independent ownership only when launched at top level.

FT-015's former unverified abrupt-owner boundary is removed for the locally proven
Linux path when the exact managed-process capability probe passes. Windows and macOS
remain development work until native execution and an unforgeable cleanup-authority
design are both proven.

## Validation Gates

- Deterministic process tests kill the interactive owner without running destructors,
  launch a descendant that calls `setsid`, and prove cleanup completes before terminal
  workload publication.
- Generation and lease races prove stale supervisors, reused process identifiers, and
  replacement cleanup records cannot affect a newer execution.
- Launch failpoints prove partial request delivery and completed request writes cannot
  terminalize without cleanup or launch-abort authority.
- Linux tests kill the supervisor itself and prove restart reconciliation waits for the
  namespace root to disappear before completing cleanup.
- Linux startup-race tests kill the supervisor before bwrap spawn, after bwrap spawn but
  before PID-1 discovery, and after the PID-1 command gate reports readiness but before
  Running publication. They prove the fixed gate prevents workload execution, recorded
  namespace roots use exact pidfd cleanup, and each terminal launch abort carries only
  bounded workload-never-launched authority.
- Bounded maintenance tests prove transaction-peak reservation, atomic
  evacuation/finalization, deletion-quarantine recovery, aggregate limits, and typed
  proof-bound retirement from a locked full workload record.
- Structural and runtime tests prove foreground terminal, MCP, raw Git, managed Git,
  skill, and agent-loop launch paths inherit the registered worker scope. Durable
  background workers prove they remain independently owned.
- Production containment preflight probes only bwrap and the exact managed-process
  backend. Git availability remains a separate diagnostic so a blocked Git executable
  cannot delay entry into the cancellable worktree path.
- `task test`, `task check`, `task coverage`, `task build`, Linux release-binary smoke,
  and the Windows/macOS CI matrix pass on the reconciled tree.

## Acceptance Criteria

- [x] Owner EOF is observed by an independent supervisor even when the nib owner is
  killed without running destructors.
- [x] Cleanup ownership is persisted and generation-fenced; reconciliation cannot
  terminalize a record while the cleanup lease is live or cleanup/launch-abort authority
  is absent.
- [x] Linux tests launch a real descendant that calls `setsid`, kill the owner, and prove
  the PID-namespace or cgroup scope terminates and reaps it before terminal publication.
- [ ] Windows tests prove abrupt owner exit closes the Job Object and terminates the full
  descendant tree before terminal publication.
- [ ] macOS behavior is tested against the documented group-contained contract and never
  claims arbitrary detached-descendant cleanup.
- [x] Foreground terminal, MCP, raw Git, managed-worktree Git, skill, and agent-loop
  children execute beneath the same registered scope root. Durable background workers
  are explicitly excluded and retain their own reconciliation.
- [x] Managed-process availability checks do not execute Git; MCP cancellation remains
  bounded when every Git invocation is replaced by a non-cooperative descendant tree.
- [x] Process identifiers are guarded against reuse, cleanup is bounded and audited, and
  stale supervisors or generations cannot affect a newer execution.
- [x] Post-handoff launch failures remain nonterminal until exact cleanup or launch-abort
  authority exists; pre-delivery failures can remove Prepared state only after bounded
  supervisor reap.
- [x] Linux supervisor crashes are recovered under the exact cleanup lease by signalling
  only the revalidated namespace-init generation and completing only after both recorded
  process generations disappear.
- [x] Process-scope storage is aggregate-bounded, restart-recovers deterministic scratch,
  and retires Complete records only from exact cleanup or launch-abort authority embedded
  in terminal workload state.
- [ ] `task test`, `task check`, `task coverage`, Linux/macOS/Windows CI, and abrupt-owner
  release-binary smoke all pass.

## Risks And Tradeoffs

- A supervisor and worker protocol adds process, persistence, and upgrade complexity.
- PID namespaces may be unavailable in restricted Linux environments; strong mode must
  fail closed or use a proven cgroup backend rather than silently weakening.
- macOS cannot match Linux/Windows for arbitrary detached descendants, so the product
  contract and diagnostics must remain platform-specific and explicit.
- Supervisor crashes require their own bounded recovery protocol and cannot be treated as
  successful cleanup merely because both leases disappeared.

## Decisions

- Linux prefers the existing bwrap PID-namespace capability. Strong containment fails
  closed when neither that backend nor a proven delegated cgroup-v2 scope is available.
- Managed-process availability is probed independently from broad sandbox diagnostics;
  the production delegation preflight must not execute Git or any other unrelated tool.
- Production delegation is Linux+bwrap only. Windows Job Object and macOS process-group
  implementations remain native mechanism tests until cleanup authority is inaccessible
  to the worker and crash recovery can be independently proven.
- A cleanup proof binds the execution generation, cleanup-lease identifier, backend,
  direct-child process identity, terminal outcome, and completion timestamp. A process
  identifier alone is never sufficient; the supervisor must persist the proof before
  releasing its lease.
- Durable background tasks do not enter this foreground supervisor protocol and retain
  their existing worker lease and reconciliation model.

## Local Implementation Evidence (2026-07-16)

- Hidden `subagent-supervisor` and `subagent-worker` processes separate interactive
  ownership, cleanup ownership, and execution. The owner retains the supervisor stdin
  pipe after the bounded launch request; EOF and a cancellation frame are observed by an
  independent supervisor thread. The control guard is created before its Tokio monitor
  future is submitted, so runtime destruction sends cancellation even if that future was
  never polled; ten consecutive runtime-drop regressions reached proof-backed
  `cancelled` records without leaving a supervisor tree.
- Version 2 process-scope records persist execution generation, cleanup-lease identity,
  backend, owner/supervisor/namespace-root process identities, cleanup state, and one
  exact cleanup or launch-abort proof. Version 1 process scopes fail closed because
  their direct-child field used the outer-monitor meaning. Scope mutations use
  exact-generation CAS under one fixed durable store lock; cleanup leases use kernel
  locks, lease release rereads the authoritative completed proof, and bounded
  maintenance recovers CAS/deletion scratch and retires only from an identical proof
  embedded in terminal workload state.
- Linux launches the production worker through
  `bwrap --unshare-pid --die-with-parent --info-fd ... --as-pid-1 --sync-fd ...` and a
  fixed shell gate. The bounded handshake validates nested namespace PID 1 and waits for
  the gate before Running publication. Supervisor EOF before publication aborts without
  executing user code; normal and restart cleanup after publication use an exact pidfd
  signal and verify that identity is gone. Production availability is cached only after
  this exact launch and pidfd-kill probe succeeds.
  Windows suspended Job assignment and the macOS dedicated process group remain native
  backend mechanisms; production delegation rejects those hosts before state creation.
- `cargo test --lib sandbox::process::tests -- --test-threads=1` passes 13/13, including
  PID-reuse markers, stale generation/snapshot fencing, exclusive cleanup ownership,
  rejection of Prepared recovery while the exact supervisor is still live and
  post-exit acquisition of the exact cleanup lease,
  forged-proof rejection, exact transaction-peak accounting, atomic
  rollback/finalization, version-1 target and transaction-scratch isolation, typed
  proof-bound retirement, deletion-quarantine recovery, failed bwrap-info handshake
  cleanup, preservation of a locked live cleanup-lease quarantine, and a real `setsid`
  descendant.
- `cargo test --test managed_process_supervisor -- --test-threads=1` passes the Linux
  multi-process owner-kill regression. It kills the owner's process group, leaves the
  independent supervisor alive, and observes terminal publication only after the PID
  namespace is reaped and the cleanup lease is absent.
- `cargo test --test managed_process_supervisor_recovery -- --test-threads=1` passes
  4/4 supervisor-loss cases. Running recovery reacquires the exact cleanup lease and
  forces the recorded namespace PID 1 to remain live until the recovery path
  exact-signals it. Prepared recovery kills the supervisor before bwrap spawn, after
  bwrap spawn but before PID-1 discovery, and after the PID-1 gate is ready but before
  Running. Every Prepared case proves user code never started and publishes only
  launch-abort authority; the recorded-root case additionally proves exact pidfd
  termination before completion.
- `cargo test --test managed_process_launch_fencing -- --test-threads=1` passes 2/2.
  The launch-fencing matrix covers partial request delivery, a complete flushed request,
  proof-free recovery evidence, and eventual proof-backed terminal publication. The
  end-to-end case reconciles a pre-Running supervisor loss as failed with
  `launch_abort_verified=true`, `workload_never_launched=true`, and no cleanup claim,
  then retires the scope from that exact authority.
- `cargo test --lib terminal_scope_retirement -- --test-threads=1` passes 6/6. It
  accepts exact cleanup or launch-abort authority through all supported terminal,
  verification, and merge result shapes; retries after transient retirement failure;
  and rejects generation, ownership, verification, status, mixed-proof, and
  stale-snapshot forgeries.
- `cargo test --lib sandbox::tests -- --test-threads=1` passes 15/15 and `cargo test
  --bin nib skill_cmd::tests -- --test-threads=1` passes 26/26. Their Unix success-path
  regressions prove lingering owned process-group members are signalled while the
  exited leader remains waitable and that a consumed child identity discards stale
  numeric group authority. Windows skill commands now retain the existing suspended
  Job Object through checked terminate-and-empty cleanup; the native descendant fixture
  remains unexecuted on this Linux host.
- `cargo test --lib sync_managed_ -- --test-threads=1` passes 3/3 for synchronous managed
  Git cancellation, drop cleanup, and stale-group-authority fencing. The focused
  successful-leader regression also proves a lingering group member is terminated
  without waiting on inherited output pipes.
- `cargo test --lib integrations::mcp_server::tests -- --test-threads=1` passes 32/32.
  EOF and fatal-input shutdown now bound cooperative subagent cancellation, hand an
  unfinished commit-aware request task to a second bounded reconciliation window, and
  report failure rather than hanging if the task remains non-cooperative.
- `cargo test --test mcp_integration -- --test-threads=1` passes 25/25. The portable
  `nib_run` cancellation fixture stalls every Git invocation and now reaches cancellable
  worktree Git without blocking on an unrelated `git --version` capability probe; that
  regression and the final-audit lock-stall regression each passed 10 consecutive runs.
- The optimized release binary passes `scripts/check-managed-process-release.sh`: a real
  parent agent launches a production subagent, the mock child creates a detached
  `setsid` process, and killing the nib owner results in verified failed reconciliation
  only after that process is gone.
- The reconciled Linux tree passes `task check`, independent `task test` with 772 tests,
  `task docs:check`, `task coverage` at 83.94 percent (53,734/64,015), the locked release
  build, strict all-target/all-feature host checks, and the managed-process release smoke.
- The worker receives `NIB_MANAGED_PROCESS_SCOPE` before any agent-loop work. Terminal
  and MCP children use the shared managed-child launcher, managed Git uses its async or
  synchronous managed launcher, and raw patch Git inherits the worker scope directly.
  The authoritative marker survives async and synchronous child environment sanitation;
  a focused core test proves background terminal work, schedules, and nested subagents
  all fail closed inside this scope.
- Native Windows and macOS owner-kill tests are present in
  `tests/managed_process_supervisor_windows.rs` and
  `tests/managed_process_supervisor_macos.rs`; they have not executed on this Linux host.

## Hosted Linux Probe Remediation (2026-07-20)

### Scope

Make the production managed-process preflight obey the existing independent-probe
decision. Keep the broad sandbox probe, including `--unshare-net`, authoritative for
general bwrap and network-boundary availability, while deriving managed-process
availability only from the exact PID-namespace launch, gate, pidfd termination, wait,
and descendant-reap probe. Keep bubblewrap's info pipe at its dynamically allocated
descriptor so Rust's internal `Command` pipes cannot collide with fixed slots. Carry the
launch fence over the child's standard input and output so every POSIX shell can enforce
it without non-portable multi-digit descriptor redirections.

### Acceptance Criteria

- [x] Production managed-process preflight does not execute the broad bwrap probe,
  `bwrap --version`, Git, or another unrelated diagnostic.
- [x] A failed broad `--unshare-net` probe can coexist with successful exact
  managed-process availability without weakening network-boundary diagnostics.
- [x] A Linux integration regression wraps bwrap, rejects only `--unshare-net`, and
  proves production preflight succeeds before broad diagnostics report their failure.
- [x] The bwrap info descriptor retains its actual allocated number across `exec`, while
  the shell gate uses standard input/output and no custom `--sync-fd`.
- [x] A subprocess regression retains the low descriptor range through descriptor 63
  before the exact probe and proves the dynamic info handoff works with `/bin/sh`.
- [x] A focused supervisor regression proves the internal launch frame cannot consume or
  alter payload stdin bytes sent after durable Running publication.
- [ ] The exact PR revision passes the hosted Validate job with required bwrap tests.

### Affected Areas

`src/sandbox/mod.rs`, `src/sandbox/process.rs`, exact managed-process capability tests,
delegation integration tests, and the Linux Validate job.

### Validation Gates

`task test:managed-process-capability`, required bwrap
supervisor/delegation regressions, `task test`, `task check`, managed-process smoke, and
the hosted Validate job.

### Local Validation Evidence

The wrapper regression passes against the local exact bwrap backend: the managed probe
does not touch `--unshare-net`, while the subsequent broad capability read reports the
injected `RTM_NEWADDR` failure and retains managed-process availability.

## Remaining Implementation Plan

1. Execute the native Windows Job Object and macOS group-contained tests on hosted
   runners and design cleanup authority inaccessible to managed workers before enabling
   either production backend.
2. Inspect the exact committed CI revision and reconcile FT-015/FT-016/T020 platform
   evidence, then move this spec to `done/` only after every criterion is proven.
