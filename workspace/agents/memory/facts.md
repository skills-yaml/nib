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

The reconciled spec lifecycle contains 18 files in `workspace/specs/done/`, 10 in
`workspace/specs/development/`, and none in `workspace/specs/backlog/`. Development spec FT-017
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

## 2026-07-21 - FT-001 native Windows completion

- Type: fact
- Source: FT-001 exact implementation-revision hosted validation
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: 2026-07-15 lifecycle count and FT-001 status only

Content:

FT-001 is complete at implementation revision
`769f67b200af70531129f7578cead29862d24c8c`. Hosted CI run `29859138441` passed
Validate, macOS Tests, and Windows Tests. The Windows job passed all 548 library tests,
15 delegation tests, nine runtime E2Es, absolute MSVC linker discovery, the real Cargo
coding E2E, release build, and release-binary help, version, and doctor smoke.

The canonical lifecycle is 19 specs in `done/`, 9 in `development/`, and none in
`backlog/`. FT-001 moved to `done/`; T006 retains its separate hosted Windows evidence
gate. Historical lifecycle entries remain historical evidence.

## 2026-08-07 - Production self-update channel is live

- Type: fact
- Source: exact hosted CI, release qualification, and production publication
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: FT-018 and T010 development status

Content:

The public `prod-latest` Release points to commit
`2ecf9d23d951a293238c46d605f2f92e3db3b946` and contains `nib-release.json` plus four
native archives and four verified checksum files. Production release run `31148222338`
was approved only after two consecutive repaired development publications and
four-platform self-update qualification run `31149505681` passed Linux, macOS Intel,
macOS Apple Silicon, Windows, and final held-production revalidation.

Official builds expose `nib update`, update only within their embedded rolling channel,
and use exact commit identity rather than only package version. Windows replacement
uses a bounded helper/worker handoff that renames the running image to a digest-bound
backup, publishes the verified candidate, reconciles crash states, and cleans exact
protocol evidence after process exit. A downloaded production Linux binary reported
the exact release identity and an already-current update made no mutation.

The reconciled lifecycle is 21 specs in `done/`, 10 in `development/`, and none in
`backlog/`; FT-018 and T010 transitioned to `done/` on this evidence.

## 2026-09-02 - Current spec lifecycle and local gate baseline

- Type: fact
- Source: development-spec closure implementation and canonical local validation
- Confidence: high
- Review: pending exact-revision independent and hosted review
- Supersedes: prior current lifecycle counts and local test baseline

Content:

The current lifecycle is 29 specs in `done/`, 16 in `development/`, and FT-020 in
`backlog/`. `task check` is the fast installer/format/Clippy loop; `task test` owns the
unchanged full serial suite; `task verify` composes each once for completion.

On the 2026-09-02 reconciled dirty tree, `task verify` passed 1,061 library, 86 CLI,
and 254 integration tests. Documentation integrity passed 5/5, host and Windows MSVC
all-target checks reached nib, runtime line coverage passed at 85.87 percent
(101,945/118,726), and optimized Linux interactive and abrupt-owner managed-process
smokes passed. Exact clean hosted evidence remains distinct and is required by the open
platform criteria.

## 2026-09-02 - Agent root futures execute on configured runtime workers

- Type: fact
- Source: T004 FT-019 Windows stack repair
- Confidence: high
- Review: pending exact-revision hosted Windows CI
- Supersedes: direct caller-thread root-future polling in run/task/subagent entrypoints

Content:

`nib run`, the durable task worker, and the subagent worker submit their owned root
future to a Tokio multi-thread runtime whose workers have 4 MiB stacks. The process main
thread blocks only on the spawned future's join handle. Join failures return bounded
static cancellation/panic diagnostics, and the subagent worker preserves its captured
optional session-lock policy across the boundary.

## 2026-09-02 - Ordinary implementation specs reconciled to native closure

- Type: fact
- Source: exact hosted CI, local Task gates, specs, and independent reviews
- Confidence: high
- Review: 2026-09-02
- Supersedes: 2026-09-02 current spec lifecycle and local gate baseline; pending
  exact-revision hosted Windows review of configured-runtime root-future execution

Content:

The canonical lifecycle is 44 specs in `done/`, T023 alone in `development/`, and
FT-020 alone in `backlog/`. Exact implementation run `33683995100` passed Validate,
macOS Tests, and Windows Tests for head
`c3b88564da4f6f654a8618e4fa544b353ece86f5` at clean merge checkout
`0479b72ad3d11fd7221632f042736b8489b6443b`. The matrix passed complete serial
suites, native all-target checks, exact release qualification, Linux/macOS terminal
smokes, Windows ConPTY and redirected smokes, and Linux managed-process owner-loss
containment. Hosted Linux runtime line coverage was 85.87 percent
(102,061/118,862). Exact binary SHA-256 values are
`e9b56b4c2b527ab04bd4e40932c83a632ae5bd5931010dee6152012b421e4276`
(Linux), `e7bbf6ea23d87a3e00b1447fc7880f2c93e6c67a27239f0068bcb599d18fb739`
(macOS), and
`e9250200aa0b06188e3e05d062ccd39115eb98311d0dc9b691cfdc5e9a324423`
(Windows).

T023 remains open because ordinary credential-free evidence cannot substitute for
owner-approved paid live qualification. FT-015/FT-017 completion retains the
Linux+bwrap-only production boundary; FT-020 owns future protected non-Linux authority.

## 2026-09-25 - FT-020 done fail-closed; T023 live unverifiable

- Type: fact
- Source: development/backlog implementation goal; live Task verdicts and GitHub env inventory
- Confidence: high
- Review: 2026-09-25 two-stage review for FT-020; T023 remains owner-gated
- Supersedes: 2026-09-02 canonical lifecycle 44 done / T023 development / FT-020 backlog

Content:

The catalog lifecycle is 63 specs in `done/`, T023 alone in `development/`, and
zero backlog specs. FT-020 shipped the Windows protected Job owner (non-inheritable
handle, DACL deny WRITE_DAC|WRITE_OWNER to Everyone) and macOS LaunchDaemon
preflight. `ProcessScopeBackend::production()` stays fail-closed on Windows and
macOS; Linux remains bwrap PID-namespace. Enabling those platforms is a later
independent native qualification, not this spec's Done bar.

T023 live catalog/canary/selected/full is unverifiable: process provider keys,
`NIB_LIVE_TESTS`/`NIB_LIVE_ACK_COSTS`, GitHub `llm-live-*` secrets/variables, and
OpenRouter `approved = true` entries are all absent. Real Task catalog with
`NIB_LIVE_TESTS=1` failed `blocked_auth` on missing `OPENAI_API_KEY`. Offline
harness 66/66 non-ignored tests passed and is not a Done substitute.

## 2026-10-05 - Workspace Docs 7 delivery and current lifecycle

- Type: fact
- Source: T062 actual integration/main events, exact hosted CI and native Task gates
- Confidence: high
- Review: independent exact-candidate spec, security and quality reviews
- Supersedes: earlier current lifecycle counts and pending T062 delivery snapshots

Content:

Workspace Docs 7 is delivered through verified main merge
`4b0245b9890bb15a4c99ff46ec7296d42bf9401f` via PR43 on 2026-10-05T20:20:23Z, following
confirmed development integration `30e9ecf8151768ca236367ed4984e1a06bc96380` via PR45.
CI 37361857315 passed Linux/macOS/Windows complete suites, exact binary
qualification, native interaction smokes and Linux coverage. Frozen native
stage `42550166c79df8d7acf4d0c405f65ce04d5947e8` passed strict ordered task check/test.
Event records receive renewed review and verification before final delivery.
The migration preserves the approved manual policy and all 70 historical done
records; canonical instructions/specs/docs/memory live under workspace/.
T063 enforces isolated read-only Git status; T065 preserves canonical LF text
and exact upstream standard bytes. T064/T066 restore existing test-fixture
contracts and have no new durable memory impact. No paid calls were made.

The canonical lifecycle has 75 done specs, T023/T061 in development and
FT-021/FT-022 in backlog. T023 retains newer protected catalog/canary evidence
and open continuation/selected/full qualification. T061 has an accepted
contract awaiting runtime implementation; released 0.2.0 membership covers
documentation only and implementation needs a fresh atomic reservation from
latest published state. The shared 0.2.0 target was applied once and development
publication was verified separately; main completion does not claim production
publication.

## 2026-10-06 - Verified T061 question-form runtime delivery

- Type: fact
- Source: T061/T067 actual branch events, independent acceptance, native Task gates and CI 37419726793
- Confidence: high
- Review: independent exact spec-compliance, quality/security/interface and Test-record reviews
- Supersedes: earlier current lifecycle counts and pending T061 implementation snapshots

Content:

T061's complete question-form contract and linked T067 runtime implementation
are delivered at `a3b99632ae8cb452d5bfaff5555a3728999e8ba5`, verified on shared
development on 2026-10-06 at 06:37:20 UTC and main at 06:40:26 UTC.
Whole-spec independent acceptance and exact frozen native/hosted qualification
passed, including Linux/macOS/Windows native interactions and 84.48% Linux coverage.
Question calls support legacy singles or sets of up to eight, described choices,
visible editors and explicit set submission. Discussion leaves linked obligations
unresolved; interruption stops the worker. Persisted exact operation/plan/invocation
identity supports conversational recovery without approving tools or waiving
verification. `/plan` and `/questions` are removed from interactive commands;
internal runtime plans remain authoritative. Legacy session defaults stay compatible.

The single minor runtime target is nib-question-form 0.3.0, applied once from
published 0.2.0 through T067. T061 retains its 0.2.0 contract-documentation history.
The current catalog has 77 done specs, T023 alone in development, and FT-021/FT-022
in backlog. T023's continuation/selected/full qualification remains open; no paid
calls or model-default changes are authorized by this delivery. Public development
and production archive publication are separately verified events, not inferred
from main delivery. Completion records receive fresh exact review and verification.

## 2026-10-06 - Verified question-form 0.3.0 development publication

- Type: fact
- Source: T067 release ledger, public nib-release.json, GitHub asset metadata and Release Artifacts 37430352520
- Confidence: high
- Review: independent publication-scope review; exact record review and fresh gates required before delivery
- Supersedes: earlier applied-only and completion-preparation publication snapshots for nib-question-form

Content:

The nib-question-form 0.3.0 reservation is released through verified development
publication at `70acf355eb55621a0206b5070b2b6205e37502ed`, published on
2026-10-06T07:50:47Z by successful Release Artifacts 37430352520. The public
manifest binds skills-yaml/nib, development-latest, version, source revision and
all four native archive sizes/digests; GitHub uploaded-asset metadata matches.
The artifact receipt is workspace/docs/work/t067-publication-reconciliation.md.
This is the existing target from baseline 0.2.0, with no second bump or change to
T067 membership or T061's published 0.2.0 contract-documentation history.

Production was separately inspected and remains 0.1.0 at
`15123a3ef275458efc87200400219aeacc3e9ea9`; development publication is not a
production rollout or updater-rollout approval. T061/T067 stay done, and T023
remains the sole development spec. No runtime behavior, provider defaults or
paid-provider qualification changes are authorized by this record update.

## 2026-10-06 - Anthropic native streamed continuation preservation

- Type: fact
- Source: T068 source inspection and credential-free conformance fixtures
- Confidence: high
- Review: 2026-10-06 independent exact-candidate review
- Supersedes: none

Content:

Anthropic streamed tool-result replay must retain the complete ordered native assistant content privately, including signed thinking (also empty displayed thinking), split opaque signatures, redacted-thinking data, native text boundaries and tool input values. Projected public events and debug output exclude those private values. T068 repairs the streaming path with deterministic HTTP round-trip and malformed/limit fixtures; the completion path already retained native content. T023's historical tool-continuation refusal used completion for both requests, so the independently evidenced streaming defect is not its established cause. Live qualification, model/default changes and paid calls remain separate from this repair.

## 2026-10-06 - Anthropic streamed repair delivered

- Type: fact
- Source: T068, PR49 and verified identical development/main refs
- Confidence: high
- Review: 2026-10-06 independent completion-record review
- Supersedes: earlier T068 implementation-only snapshot

Content:

T068's native Anthropic streamed continuation repair reached shared development and main at `9762df5c10727889adff072045fcd8a8c1369dff` on 2026-10-06 (development 2026-10-06T11:58:18.336608+00:00, main 2026-10-06T12:01:05.731122+00:00) via PR49. Exact independent code/Test reviews, native verify/docs/versions and Linux/macOS/Windows qualification passed, including 84.54% Linux coverage. Native signed/omitted/redacted blocks remain private, ordered and bounded, with malformed streams rejected before tool authority. The reconciled catalog is 78 done, T023 development and two backlog proposals. T023's completion-path refusal remains unresolved; no paid qualification or default/prompt change is implied. Publication and protected production rollout remain separate observed events.

## 2026-10-06 - Anthropic repair development publication

- Type: fact
- Source: T068 and inspected public development release manifest/asset metadata
- Confidence: high
- Review: independent publication-record review required before delivery
- Supersedes: earlier T068 publication-pending snapshot

Content:

Development publication was independently verified on 2026-10-06: Release Artifacts 37459973513 completed successfully, including all four native archive builds and publication. The public development-latest nib-release.json binds version 0.3.1 to exact revision 9762df5c10727889adff072045fcd8a8c1369dff, published at 2026-10-06T12:10:32Z. Its manifest SHA-256 is f83f51f5635cda345bbd58e153d144e49d1127c5c25bfcc5c532c76f9e43ceea; all four archive sizes and SHA-256 digests match GitHub uploaded-asset metadata. Released means development-channel publication; production was separately inspected at 0.1.0, revision 15123a3ef275458efc87200400219aeacc3e9ea9. No second bump or production rollout is implied.

## 2026-10-06 - Production 0.3.1 release

- Type: fact
- Source: [Production release run 37468381524](https://github.com/skills-yaml/nib/actions/runs/37468381524), inspected public manifest and downloaded archives
- Confidence: high
- Review: Independent exact-source and publication-evidence review before handoff
- Supersedes: Earlier current-production 0.1.0 snapshots; development publication history retained

Content:

User-requested production 0.3.1 was published at 2026-10-06T16:43:51Z from exact `61e166e2038a895996cc709aad78d066ba979eba` through the protected release workflow. The public `prod-latest` manifest SHA-256 is `da3e76a8ed6f152cc7fc1c1334f50e153ac1f7485f535a636168752328ed5138`. All four native archives and all four checksum files were downloaded and verified against the manifest and uploaded-asset metadata. The downloaded Linux binary passed the offline redirected/native-PTY interaction smoke with exact prod version/commit identity, clean-source eligibility, privacy and terminal restoration. Independent exact-source review, frozen native gates, all-platform CI and all four production builds passed before publication approval.

This publishes the existing 0.3.1 target without another bump or runtime/model/prompt change. T023 remains open for protected live-provider acceptance; no paid call or live-provider pass is inferred from this production release.

## 2026-10-06 - Public catalog refresh and provider-controlled Anthropic thinking

- Type: fact
- Source: user + T069 and current first-party provider documentation
- Confidence: high
- Review: Independent exact-candidate review required before handoff
- Supersedes: Older bundled picker defaults; historical T023 evidence retained

Content:

T069 refreshes the advisory catalog from public sources inspected on 2026-10-06:
OpenAI GPT-6.1 Sol, Anthropic Claude Opus 5.5 (direct API `claude-opus-5-5`),
Google Gemini 3.8 Flash and Grok 4.7 become bundled defaults; OpenRouter uses its
separately verified canonical IDs, including dotted Anthropic versions. Explicit
user model selections and picker overrides remain authoritative.

Bounded Anthropic text requests retain the provider's thinking default because
Opus 5.5 cannot disable thinking. The existing max_tokens ceiling bounds total
thinking plus text, and exhaustion remains a failure without executable tool
authority. Native thinking, signatures and redacted blocks stay private and
unchanged during tool continuation. Successive tool batches retain every earlier
native assistant/result pair to preserve signed prefixes; cumulative history is
bounded by 256 items and 4 MiB with fail-closed limits. Native continuation
also retains the original fallback-system-prompt choice when a Required tool
turn continues with Auto. Offline matrix fixtures account for curated models and
separately approved IDs; actual qualification still requires canonical catalog
visibility. This catalog refresh does not alter T023's
protected selected model matrix or OpenRouter allowlist, and no paid generation
or live qualification is inferred. Shared integration, main delivery and
publication remain separately evidenced events.

## 2026-10-08 - Shared 0.4.0 catalog and sandbox integration

- Type: fact
- Source: verified remote development 81c37a1908ec1032809e97cdf0b0b39d5253603b; release ledger and T069-T073 integration records
- Confidence: high
- Review: independent exact spec-compliance and correctness review approved 81c37a1; fresh complete native gates passed
- Supersedes: catalog integration-pending snapshots and catalog/sandbox release-scope question

Content:

The catalog and Anthropic continuation work is integrated with T080 phase 1
in shared development under the single minor 0.4.0 release. The release ledger
includes T069-T073 in test and T080 in development; native mirrors use the
same target without another bump. T073 governs the final catalog selection.
T080's later phases, main delivery, publication of this combined revision
and T023 live-provider acceptance remain separate.

## 2026-10-09 - Progressive skill management

- Type: fact
- Source: T083 implementation and regression fixtures
- Confidence: high
- Review: independent spec-compliance and security/interface review approved the implementation

Content:

Nib shares one bounded catalog between listing and runtime discovery, including
repository `.agents/skills` folders and linked SKM skills. Bounded catalogs keep
repository root precedence even when link targets sort after user skills. Turns advertise metadata;
explicit `$name`, configured/profile activation or `load_skill` loads instructions.
Activated bodies remain complete in prompts, and activation records session usage
before installing restrictions and hooks. Disabled controls suppress profile and
configured activation; explicit requests for disabled skills report an error.
Implicit-disabled skills require explicit selection. Supporting files are bounded
regular reads beneath an activated root. Discovery refreshes each user turn.
