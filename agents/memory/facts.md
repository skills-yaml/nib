# Facts

## 2026-07-15 - Current Rust runtime boundaries

- Type: fact
- Source: repository audit
- Confidence: high
- Review: none
- Supersedes: none

Content:

Sessions, plan steps, memory, and daemon workload records are profile-scoped under
`.nib/profiles/<id>/` by default. Both directions of shipped MCP support use stdio.
External messaging providers integrate through `src/integrations/gateway.rs`; nib
does not own provider authentication, listeners, or reply delivery.

Project standards and library documentation are discovered only from fixed local
roots, without following symlinks, and are included under explicit file/count/byte and
aggregate model-context budgets. Skill usage is aggregated from authoritative
profile-session records for curator retention. AGENTS instructions may select only a
configured named boundary profile that monotonically tightens the base execution
boundary.

Gateway deliveries for the same external conversation serialize on a process-visible
lock whose persistent anchor lives above the replaceable sessions directory. Project
documentation discovery retains at most its configured entry cap before sorting.
Repository-wide subagent merge and recovery serialize on a persistent hardlink anchor
under `.nib/`, outside the replaceable `.nib/subagents/` records directory.

Rolling release publication is serialized per channel and uses fixed staging/backup
refs plus a versioned Release-body marker for process-loss recovery. Local publication
tests cover complete, partial, ambiguous, killed, read-error, retagged, and compound
failure states. GitHub Release writes themselves have no conditional compare-and-swap,
so the repository workflow is the documented exclusive rolling-release writer.

## 2026-07-15 - Audited spec lifecycle and process-containment boundary

- Type: fact
- Source: repository audit
- Confidence: high
- Review: none
- Supersedes: earlier 2026-07-15 current lifecycle counts

Content:

The reconciled spec lifecycle contains 18 files in `docs/specs/done/`, 10 in
`docs/specs/development/`, and none in `docs/specs/backlog/`. Development spec FT-017
owns durable abrupt-owner descendant-process containment. Its local Linux supervisor,
PID-namespace, cleanup/launch-abort authority, generation fencing, and real owner-kill
tests are implemented. The launcher persists the exact supervisor identity before any
request byte. Linux schema-v2 scopes bind cleanup to the validated namespace PID 1, use
an EOF-sensitive post-init PID-1 command gate until that identity is durable, and use
exact pidfd signalling for normal and crash recovery. A supervisor loss while the scope
is still Prepared publishes a distinct proof that the gated workload never launched;
it does not claim descendant cleanup. Complete scopes retire only from matching proof
authority embedded in the locked full workload record, including retry after later
verification or merge statuses. Production eligibility is cached only after the same
info/socket/pidfd-kill protocol succeeds. Version-1 scope state is preserved and
rejected per scope without blocking unrelated version-2 work. Unix launchers retain
process-group authority only for groups they created and pin an exited leader with
`waitid(..., WNOWAIT)` while signalling lingering members before final reap. Production
delegation is Linux+bwrap only; Windows Job and macOS group-contained backends remain
non-production native mechanism tests until cleanup authority is inaccessible to
managed workers. A cleanup-lease deletion quarantine remains `Live` while its exact
file lock is held and cannot be recovered or retired out from under the finalizing
owner. A legacy running record without a process scope remains nonterminal with
`recovery_required` evidence rather than claiming unverified cleanup.

## 2026-07-16 - Local stdio MCP lifecycle guarantees

- Type: fact
- Source: repository audit and canonical local validation
- Confidence: high
- Review: none
- Supersedes: none

Content:

On Linux, outbound MCP startup succeeds only after the initialized notification is
written to the child transport. Fatal reader or writer failure closes new and queued
requests, resolves pending requests once, terminates descendants that remain in the
managed process group, and reaps the direct child. Configured sensitive values are
normalized and redacted from returned transport, RPC, and stderr errors; secret-bearing
successful tool metadata is rejected atomically.

The inbound stdio server keeps consuming bounded frames while requests execute,
supports targeted cancellation, cancels and joins active work on EOF or fatal input,
bounds stdout backpressure, arbitrates one response per request, and reconciles
cancelled subagent work against durable state. A non-cooperative subagent request gets
one bounded cooperative join and a second bounded commit-aware handoff; shutdown
reports failure rather than waiting indefinitely. Generic cancellation stops execution
before audit persistence, drives shutdown cancellations concurrently, and bounds its
session-lock write and authoritative reread with one absolute deadline. Windows Job
Object runtime behavior was not executed on this host, macOS MCP runtime behavior was
also not executed, and both remain explicit development gates.

Inbound request lifecycles and their spawned subagent loops inherit a finite SessionStore
lock policy that moves synchronous waits off the multi-thread Tokio worker. Cancellation
reserves an audit identity before session initialization, can idempotently create that
session during reconciliation, and gives explicit cancellation and the Drop fallback
distinct atomic ownership states.

## 2026-07-16 - Durable managed-worktree ownership

- Type: fact
- Source: FT-015 implementation and focused validation
- Confidence: high
- Review: none
- Supersedes: process-local managed-worktree ownership boundary

Content:

Subagent and session worktree intent, generation, artifact identities, branch lineage,
and cleanup phases are CAS-persisted under project `.nib/worktree-ownership/`. Restart
recovery promotes a reciprocal completed creation, compensates an incomplete intent,
rehydrates receipt-bound staged/final artifacts and hard-link ref anchors, or fails closed
on identity replacement and quarantine-only physical cleanup. Complete tombstones are
compacted under a crash-recoverable persistent kernel-lock domain. The retained namespace
is bounded to 64 records and 256 MiB, with deterministic transaction scratch bounded to
512 MiB and recovered before compaction; collected records use deadline-bound
non-destructive absence proof. Focused Linux validation covers process-state loss,
manager restart, pre/post staging-CAS crashes, cleanup-phase and anchor-only recovery,
quarantine reporting, missing generational receipts, adopted revisions, prior-anchor
retirement, same-content path/ref replacements, stale CAS recovery, killed lock holders,
and compaction idempotency. Exact namespace detachment is the supported Unix contract;
hostile same-UID replacement is outside the isolation boundary. Hosted Windows/macOS
gates remain open.

Branch cleanup holds both the packed-ref and target-ref lock domains while rechecking
loose and packed namespace state; any packed exact/ancestor/descendant conflict preserves
the loose ref or retained anchor and remains nonterminal. Incomplete creation intents can
mark registration cleanup `Removed` only after a bounded comparison with the persisted
pre-add snapshot proves no post-snapshot admin entry; unattributed partial-add
registrations remain preserved and reported.

Initial branch staging is identity-CAS-persisted before Git protocol locks are acquired.
Managed packed/target locks contain receipt, reference, and role markers and retain a
kernel lock; restart removes only dead matching locks (including legacy deletion
quarantines), defers valid foreign owners, and preserves live or ambiguous state. Durable
ownership CAS uses canonical JSON as its commit point: target-missing evacuation restores
the validated previous record, while target-present publication retires it. Cleanup never
persists branch `Removed` or ownership `Complete` until both protocol locks are physically
released. Linux child-kill regressions cover both CAS windows, pre-stage scratch, and
post-target lock recovery.

## 2026-07-16 - Exact session audit floats and supervisor teardown

- Type: fact
- Source: canonical-gate failure analysis and focused stress validation
- Confidence: high
- Review: independent quality audit
- Supersedes: none

Content:

SessionStore post-publication verification depends on exact finite IEEE-754 readback.
The workspace enables serde_json's `float_roundtrip` feature; otherwise audit timing
values can parse one ULP away from the exact bytes and falsely fail structural
verification. The regression value `1.5519787360000001` is checked in both the typed
tool duration and nested JSON, and readback errors report only the mismatched field.

The subagent supervisor control guard is constructed before its Tokio monitor future is
submitted. Dropping a runtime therefore sends explicit cancellation even if the future
was never polled, and durable cancellation is published only after descendant cleanup
proof. When a legacy durable record cannot be reconciled, an exact locally tracked task
is still aborted, but the durable record remains `running` and the API reports the
cancellation as unresolved rather than claiming a stopped workload without proof.

Production subagent containment checks only bwrap and the exact managed-process backend.
Broad sandbox diagnostics may probe Git separately, but the delegation preflight must
not execute Git before cancellable worktree creation; otherwise a non-cooperative Git
executable can make MCP cancellation unresponsive before cancellation ownership exists.

## 2026-07-16 - Exact plan binding and complete executor audit

- Type: fact
- Source: done-spec remediation and canonical validation
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: none

Content:

Every active session run holds one OS-backed lease from before its first mutation
through final reconciliation. A plan is resumable only when its immutable ID,
normalized goal, cursor, approval state, and step-state ordering form a valid incomplete
structure. Approval decisions, tool outcomes, questions, and reconciliation updates use
that exact persisted plan identity; a stale actor is audited and cannot approve or
advance a replacement plan. A completed plan cannot authorize another mutation.

Executor calls without an operational session still create or reuse a profile-scoped
implicit audit session and persist redacted attempt and outcome evidence. The implicit
session is audit-only: it does not supply plan authority or become the origin for
scheduled or background work. Persisted tool audit linkage accepts only the identified
plan loaded from the authoritative session, never a caller-supplied `plan_id`.

Strict skill inventory fails closed on incomplete traversal, count truncation, malformed
manifests, and non-regular `SKILL.md` entries. Run and chat share one serialized console
input source for approvals and questions; closed input reconciles the active session
without introducing an invalid message-role transition.
