# Memory Changelog

## 2026-08-20 - FT-019 presentation and delivery decisions recorded

- Type: decision
- Source: user
- Confidence: high
- Review: none
- Supersedes: none

Content:

Updated `workspace/specs/backlog/ft_019_codex_inspired_chat_and_tui_interactions.md` with
TUI ledger presentation, shared view-model ownership, queue-first live input, default
key semantics, and remaining open decisions. Spec stays in backlog.

## 2026-06-17 - Initialize Agent Memory

- Type: fact
- Source: user
- Confidence: high
- Review: none
- Supersedes: none

Content:

Initialized `workspace/agents/memory/` for nib during additive adoption of `workspace-docs@1.0.0`.

## 2026-06-20 - PR #1 Merged: Rust CLI + LLM Agent Loop

- Type: release / milestone
- Source: merge
- Confidence: high

Content:

Pull request #1 (feat/implement-basic-agent-tools) merged into main.

Major deliverables:
- Full Rust CLI port (auth, chat with /model only, run, etc.).
- LLM integration (multi-provider via LiteLLM) + core agent loop.
- Per-project .nib/ sessions and config.
- Hybrid sandbox foundations and specs FT-003 / FT-004 marked done.
- skm-style CI, installers, and Task integration.

See decisions.md for details. Merge commit: e47cb7f.

## 2026-07-02 - FT-005: Pure Rust core implemented

- Type: implementation / milestone
- Source: FT-005 Phases 0–6
- Confidence: high

Content:

- Migrated agent loop, ToolExecutor, all 5 core tools, LLM providers (OpenAI, Anthropic, Gemini, Grok, OpenRouter, Mock), context (AGENTS.md), sandbox detection, ratatui TUI stub, and `nib doctor` to Rust.
- Removed Python core (`src/nib/`, `pyproject.toml`); `nib chat` / `nib run` run in-process.
- 15 Rust tests passing; `task check` green.

## 2026-07-15 - Done-spec implementation audit

- Type: process / validation
- Source: repository audit
- Confidence: high

Content:

Audited every spec in `workspace/specs/done/` against current code, tests, documentation,
and external release evidence. Unsupported completion claims were reopened in
`development/`; duplicate feature IDs were corrected; `task docs:check` was added to
prevent broken links, duplicate IDs, and unchecked done-spec acceptance items.

## 2026-07-15 - Development-spec implementation reconciliation

- Type: process / validation
- Source: repository audit
- Confidence: high

Content:

Reconciled all 25 files in `workspace/specs/development/` against the Rust source and test
tree. Every spec now has explicit Development status and an authoritative scope,
acceptance, affected-area, evidence, gate, and gap section. No spec moved to `done/`.
The runtime coverage gate passed at 83.00 percent, and `task docs:check` passed all
five documentation integrity tests; aggregate repository gates remain for final
integration.

## 2026-07-15 - Done-spec implementation remediation completed

- Type: implementation / validation milestone
- Source: repository audit and canonical Task gates
- Confidence: high

Content:

Closed every feasible in-repository implementation gap found while auditing the 27
completion claims. Twenty-four specs are verified in `done/`. FT-001 and T006 remain
in `development/` until the configured Windows CI job executes successfully. T010
remains there because the exact current release-workflow revision needs a committed
development-channel run and GitHub's Release mutation API cannot fence simultaneous
external retagging; the project must either exclude external writers or adopt an
immutable, Git-CAS-controlled channel pointer. FT-015's repository merge lock now uses
a persistent `.nib` hardlink anchor, closing the last replaceable-lock-domain finding.
Current local evidence is 453 deterministic tests, a green post-transition
`task check`, 84.27 percent runtime line coverage, the locked optimized build, and
isolated release smoke for version, project-doc context, and doctor.

## 2026-07-15 - Final quality review reopened durable state and MCP lifecycle specs

- Type: implementation / security review
- Source: final two-stage review
- Confidence: high

Content:

The final spec-compliance review passed, but the subsequent code-quality/security
review found replaceable durable/daemon lock domains, a non-resumable `reconciling`
task state, MCP startup errors that could precede configured-secret redaction, and an
inbound MCP server that could not consume cancellation while a tool was active. T004,
T020, and FT-016 returned to `development/` with explicit remediation and validation
criteria. The release transaction review was clean after its backup-only recovery fix.

## 2026-07-15 - Provisional done-spec audit reconciliation

- Type: process / documentation reconciliation
- Source: repository audit
- Confidence: high

Content:

Reconciled the lifecycle inventory to 18 specs in `done/`, 9 in `development/`, and 1
in `backlog/`. This supersedes the earlier 24-done and 21-done current-state claims.
T003, T004, T007, and FT-015 now state that conditional namespace quarantine does not
prove exact Unix physical unlink. FT-017 owns stronger abrupt-owner descendant-process
containment. Historical test and coverage numbers remain labeled as historical; final
canonical Task gates, platform gates, coverage, and release smoke are still pending and
this entry does not claim a green reconciled tree.

## 2026-07-16 - Final spec reconciliation and local validation

- Type: process / validation
- Source: repository audit and canonical Task gates
- Confidence: high
- Review: none
- Supersedes: 2026-07-15 provisional reconciliation and earlier current-state gate claims

Content:

The reconciled lifecycle is 18 done, 9 development, and 1 backlog. Local stdio MCP
transport, redaction, metadata, cancellation, EOF, backpressure, and reconciliation
gaps were closed; managed-worktree cleanup now removes exact-owned artifacts and
preserves or reports unproven state.

The Linux tree passed `task check`, independent `task test` with 700 top-level tests,
85.21 percent coverage (45,586/53,499), all five `task docs:check` invariants, the
locked release build, strict format/check/Clippy/diff gates, release-binary smoke, and
raw-PTY interaction smoke. Windows and macOS runtime validation were unavailable and
remain open, along with FT-015 ownership-provenance limits and the other documented
development and backlog boundaries.

## 2026-07-16 - Post-validation audit reopened managed-process remediation

- Type: process / validation
- Source: independent spec and code-quality review
- Confidence: high
- Review: none
- Supersedes: 2026-07-16 final spec reconciliation and local validation

Content:

The current lifecycle is 18 done, 10 development, and 0 backlog. FT-017 remains in
development while process-state publication bounds, proof-bound retirement, and the
Linux namespace-root readiness and recovery boundary are reconciled. Earlier local
gate counts are historical evidence for the prior tree; canonical Task, coverage,
release-smoke, and final review gates must be rerun after this remediation.

## 2026-07-16 - Managed-process remediation revalidated locally

- Type: implementation / validation
- Source: canonical Task gates and independent two-stage review
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: 2026-07-16 post-validation audit reopened managed-process remediation

Content:

The lifecycle remains 18 done, 10 development, and 0 backlog. The Linux managed-process
remediation now includes exact gated namespace startup, proof-bound scope retirement,
runtime-drop cancellation established before task polling, bounded MCP shutdown handoff,
exact session float readback, and managed-process-only production capability preflight.
Legacy local tasks are stopped when possible without falsely terminalizing unreconciled
durable records.

The reconciled Linux tree passed `task check`, independent `task test` with 772 tests
(588 library, 53 CLI, and 131 integration), all five `task docs:check` invariants,
83.94 percent runtime line coverage (53,734/64,015), the locked optimized build, strict
host all-target/all-feature checks, and the real abrupt-owner managed-process release
smoke. Native Windows and macOS execution remains open; local cross-target checks stop
in `ring` because the MSVC librarian and Apple C toolchain/SDK are unavailable.

## 2026-07-16 - Done-spec audit completed with exact plan and audit ownership

- Type: implementation / validation
- Source: done-spec audit, remediation, and canonical Task gates
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: 2026-07-16 managed-process remediation revalidated locally

Content:

The audit of all previously completed specs is reconciled to 18 specs in `done/`, 10
in `development/`, and none in `backlog/`. Nine reopened specs returned to `done/`
after feasible gaps were implemented; the remaining ten retain explicit remote,
Windows, macOS, or stronger platform-authority gates.

Final remediation added whole-run session leases, immutable plan identity and normalized
goal binding, strict plan-structure validation, stale-approval compare-and-set behavior,
completed-plan mutation denial, mandatory profile-scoped implicit executor audit,
authoritative-only plan audit linkage, strict skill inventory errors, shared console
question input, and a real edit/compression/`cargo test` agent-loop scenario.

The reconciled Linux tree passed `task check` and independent `task test` with 795 tests
(601 library, 61 CLI, and 133 integration), plus 83.90 percent runtime line coverage
(55,083/65,656). Final documentation, optimized-build, managed-process release smoke,
strict all-target/all-feature, formatting, and diff checks are recorded by the current
validation run.

## 2026-07-21 - FT-001 transitioned to done

- Type: documentation / validation
- Source: exact implementation-revision hosted CI and lifecycle reconciliation
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: current FT-001 development status and 18/10/0 lifecycle count

Content:

Moved FT-001 from `development/` to `done/` after hosted run `29859138441` passed the
exact implementation revision `769f67b200af70531129f7578cead29862d24c8c` on Linux,
macOS, and Windows. Updated inbound links, lifecycle inventory, final native Windows
evidence, and FT-015's bounded-cleanup follow-up evidence.

The reconciled lifecycle is now 19 specs in `done/`, 9 in `development/`, and none in
`backlog/`. T006 remains pending its independent hosted Windows evidence reconciliation.

## 2026-08-07 - FT-018 self-update production rollout completed

- Type: release / validation milestone
- Source: exact hosted CI, release qualification, and production publication
- Confidence: high

Content:

Released the update-capable production build at commit
`2ecf9d23d951a293238c46d605f2f92e3db3b946`. Two consecutive repaired development
publications established the bootstrap and candidate; read-only run `31149505681`
proved startup notification, exact self-replacement, current-build no-op, and cleanup
on Linux, both macOS architectures, and Windows. Exact production run `31148222338`
then published the coherent nine-asset `prod-latest` Release. FT-018 and T010 moved to
`done/` after public manifest, checksum, binary identity, and no-op smoke validation.
The resulting lifecycle is 21 done, 10 development, and 0 backlog.

## 2026-08-19 - T029 self-update channel switching implemented locally

- Type: implementation / validation milestone
- Source: T029 and local canonical gates
- Confidence: high
- Review: spec-compliance and code-quality/security self-review

Content:

Official managed builds now accept `nib update --channel prod|development`, with
`production` and `dev` aliases. Target manifest, archive, checksum, and staged embedded
identity remain bound to the selected compile-time channel; explicit cross-channel
requests replace same-commit builds so the installed binary durably owns subsequent
channel selection. Option-free updates and unmanaged installer guidance are unchanged.

Focused updater tests, `task docs:check`, post-review `task check`, independent
`task test`, 84.00 percent runtime line coverage (65,480/77,952), the locked optimized
build, and diff validation passed locally. The first coverage attempt exposed a
transient unrelated Linux managed-process recovery failure; the exact test passed in
both normal suites and the final instrumented rerun. T029 remains in development
pending exact native hosted CI and a real managed cross-channel round trip.

## 2026-08-19 - T029 development channel release published

- Type: release / validation milestone
- Source: PR #21, hosted CI, release run, and public artifact smoke
- Confidence: high
- Review: hosted Linux, Windows, macOS, and release transaction

Content:

PR #21 passed hosted CI and merged to `development` as
`c7ee849c669c9e93ec96281a602f928ae31a23cb`. Release run `32256869402` published the
complete nine-asset `development-latest` prerelease for all four supported native
targets. A public Linux archive passed its checksum, reported the exact development
identity, switched through `nib update --channel prod` to production commit `79ea99d`,
and then completed an option-free production no-op. T029 remains in development until
the new command is published to production and the reverse production-to-development
switch is proven.

## 2026-08-19 - Interactive plain/TUI parity completed

- Type: implementation / validation milestone
- Source: user + T028 + T030
- Confidence: high
- Review: independent spec-compliance and code-quality self-reviews

Content:

Plain and full-screen interaction now share one command registry, bounded completion,
session projection, strict preview-confirm switching, model/provider, skill, MCP,
approval, question, streaming, cancellation, and reconciliation contracts. The TUI is
current-session-first, and late events remain bound to their launch session. `nib` and
`nib chat` use one automatic interactive launcher with `--plain` and `--tui` overrides;
`nib tui` is a compatibility alias and `nib run` remains one-shot.

T028 moved to `done`. T030 moved to `done` after hosted PR run `32312998166` passed the
exact implementation revision on Linux, macOS, and Windows, including both native
release-binary smoke jobs and the Windows pseudoterminal probe. Local evidence includes
green `task check`, independent `task test`,
`task check:all-targets`, all five documentation checks, 84.18 percent runtime line
coverage (67,105/79,718), the locked release build, and Linux release-binary smoke for
plain/TUI selection, approval, question, completion, session switching, cancellation,
workload routing, and terminal restoration. One independent-suite attempt hit an
unrelated namespace-recovery timing assertion; that exact test passed in the preceding
canonical suite, the clean rerun, and the instrumented coverage suite.

## 2026-09-02 - Development-spec closure remediation prepared

- Type: implementation / documentation / validation milestone
- Source: user-requested development-spec review and closure actions
- Confidence: high
- Review: pending exact-revision independent and hosted review

Content:

Implemented T004's caller-stack-independent root runtime boundary for `nib run`, durable
task workers, and subagent workers. Reconciled an exact cleanup-lease finalization race
that surfaced in the first complete suite, added deterministic disappearance coverage
and `task test:delegation`, separated fast `task check` from the full `task verify`
aggregate under T035, and changed all native CI jobs to exact release-binary LLM
qualification. FT-015 now explicitly closes as a Linux-production-only v1 contract;
FT-020 records protected non-Linux production delegation authority as future backlog.

The final local gates passed: `task verify` (1,061 library, 86 CLI, 254 integration),
documentation 5/5, host and Windows MSVC all-target checks, 85.87 percent runtime line
coverage, locked optimized build, Linux interactive PTY smoke, abrupt-owner
managed-process smoke, dirty-tree release-harness execution, and patch hygiene. T023's
offline implementation is green but no paid/live authority was inferred; its external
evidence remains open.

## 2026-09-02 - Native matrix closed 15 development specs

- Type: implementation / validation / lifecycle milestone
- Source: PR #25, run 33683995100, local Task gates, and two-stage review
- Confidence: high
- Review: independent spec-compliance and code-quality/security reviews

Content:

Hosted Linux, macOS, and Windows passed the exact implementation revision, including
complete serial suites, all-target checks, exact optimized-binary qualification,
platform interaction smokes, coverage, and Linux managed-process owner-loss proof.
Late hosted timing races were confined to phase-specific test fixtures and hardened
without changing production deadlines or weakening fail-closed assertions.

Fifteen evidence-complete specs moved to `done/`: T003, T004, T006, T007, T020,
T021, T022, T026, T029, T034, T035, FT-015, FT-016, FT-017, and FT-019. The
resulting lifecycle is 44 done, 1 development, and 1 backlog. T023 remains the sole
development spec pending explicit live-provider authority and privacy-reviewed evidence;
FT-020 remains backlog for protected non-Linux production delegation authority.

## 2026-09-25 - FT-020 done fail-closed; T023 live unverifiable

- Type: implementation / lifecycle milestone
- Source: development and backlog implementation goal
- Confidence: high
- Review: FT-020 two-stage review; T023 live Task fail-closed

Content:

Moved FT-020 to `done/` with protected Windows Job owner/DACL and macOS reaper
preflight. `production()` remains fail-closed on Windows and macOS; Linux
PID-namespace production is unchanged. T023 live qualification was re-run through
the real Task targets and GitHub environment inventory and is unverifiable
without owner credentials. Catalog lifecycle: 63 done, 1 development (T023), 0
backlog.

## 2026-09-30 - T060 terminal preflight and Task executable availability

- Type: decision update
- Source: user + T060
- Confidence: high
- Review: focused terminal preflight gate, full verification, and two-stage self-review
- Supersedes: none

Content:

Recorded the stable Task listing, terminal scope recovery, and read-only
home-installed executable contracts in `decisions.md`.

## 2026-10-01 - Question-form decisions and live qualification status reconciled

- Type: decision / documentation update
- Source: user + T061/T023 spec review
- Confidence: high
- Review: user decisions; documentation validation recorded in specs

Content:

Recorded the accepted question interruption, automatic conversational resumption,
proposal chat, and spec-based planning direction. Implementation remains pending.
Recorded the decision to fix the current Anthropic continuation and superseded
obsolete missing-credentials snapshots using T023's newer retained live evidence.

Validation for this documentation reconciliation: `task docs:check` passed
all five checks; unrestricted `task verify` passed 1,252 library tests,
93 CLI tests, all integration suites, and doctests; `git diff --check` passed.
No implementation or live qualification completion is inferred.

## 2026-10-02 - Workspace Docs 7 adoption

- Type: decision update
- Source: user + T062
- Confidence: high
- Review: independent review and final verification required

Content:

Recorded the approved v7 layout, branch topology, historical-state preservation,
native gates and versioned skill configuration in decisions.md. Local migration
stays in development; integration, main merge and publication are not inferred.


## 2026-10-05 - Read-only Git status security boundary

- Type: decision update
- Source: T063 + independent review
- Confidence: high
- Review: exact-candidate review required before delivery

Content:

Recorded the strict isolation requirement and isolated-repository configuration
limit in decisions.md. This closes the host helper-execution finding discovered
while reviewing T062's combined main promotion. Integration and main delivery
remain separately verified lifecycle events.

## 2026-10-05 - Workspace checkout-byte policy

- Type: decision update
- Source: T065 + independent review
- Confidence: high
- Review: exact candidate and hosted Windows verification required before delivery

Content:

Recorded in decisions.md the LF generated-context/Workspace checkout policy and
raw-byte preservation of the imported standard. A real Git forced-CRLF fixture
passes full governance and rejects removed protection and source-byte changes;
ordinary text proves conversion is active. Main delivery remains separately
verified and no checksum gate is relaxed.

## 2026-10-05 - Verified Workspace Docs 7 main delivery

- Type: fact / lifecycle update
- Source: T062 and linked delivery repairs
- Confidence: high
- Review: independent exact source/main-candidate reviews and canonical native/hosted gates

Content:

Recorded in facts.md the actual development/main delivery, 75 done / two
development / two backlog lifecycle, preserved historical boundary and separate
version/publication events. Durable governance, isolated Git-status and checkout
policies remain in decisions.md. T023/T061 implementation/qualification remain
active; no completion or paid-call authority is inferred for them. T064/T066
memory is none because their bounded fixture repairs restore existing contracts.

## 2026-10-06 - Verified T061/T067 main delivery

- Type: fact / lifecycle update
- Source: T061/T067 independently accepted runtime and confirmed development/main events
- Confidence: high
- Review: independent exact source, whole-spec acceptance and Test-record reviews; final records require renewed verification

Content:

Appended shipped question forms, exact linked conversational recovery, compatibility,
retained execution gates and removed interactive commands to facts.md. Recorded
actual same-revision development/main delivery, the single applied runtime 0.3.0
reservation with preserved 0.2.0 contract history, and the reconciled catalog of
77 done / T023 development / two backlog specs. Paid live qualification and public
archive publication remain separate; no private session data or sample answers
are stored. Earlier accepted decisions remain preserved.

## 2026-10-06 - Question-form development publication reconciled

- Type: fact / publication update
- Source: T067 existing release phase and verified public development artifacts
- Confidence: high
- Review: independent publication-scope review; exact records and fresh verification required before delivery

Content:

Appended to facts.md the verified 0.3.0 development publication at exact
70acf355eb55621a0206b5070b2b6205e37502ed, its successful release run and four
archive metadata checks. workspace/releases.json now reconciles nib-question-form
from applied to released, preserving the target, baseline, member and old release
history. Production remains a separate 0.1.0 event; no version bump, runtime
change or paid-provider qualification is implied. Completed T061/T067 history
and the existing catalog lifecycle remain intact.

## 2026-10-06 - Anthropic native streamed continuation preservation

- Type: fact
- Source: T068 source inspection and credential-free conformance fixtures
- Confidence: high
- Review: 2026-10-06 independent exact-candidate review
- Supersedes: none

Content:

Appended the verified native Anthropic streaming preservation contract to facts.md, including opaque privacy/bounds and the distinction from T023's unresolved completion-path refusal. T068 is a compatible patch reservation; its offline fixtures do not establish live-provider qualification or authorize paid calls. No real provider content, credentials or private thought data is stored.

## 2026-10-06 - Anthropic streamed repair delivered

- Type: fact
- Source: T068, PR49 and verified identical development/main refs
- Confidence: high
- Review: 2026-10-06 independent completion-record review
- Supersedes: earlier T068 implementation-only snapshot

Content:

Appended the verified T068 codec delivery, exact acceptance/gate evidence, unchanged single 0.3.1 target and reconciled catalog to facts.md. T023 remains unresolved and no paid-provider pass or protected production rollout is inferred.

## 2026-10-06 - Anthropic repair development publication

- Type: fact
- Source: T068 and inspected public development release manifest/asset metadata
- Confidence: high
- Review: independent publication-record review required before delivery
- Supersedes: earlier T068 publication-pending snapshot

Content:

Development publication was independently verified on 2026-10-06: Release Artifacts 37459973513 completed successfully, including all four native archive builds and publication. The public development-latest nib-release.json binds version 0.3.1 to exact revision 9762df5c10727889adff072045fcd8a8c1369dff, published at 2026-10-06T12:10:32Z. Its manifest SHA-256 is f83f51f5635cda345bbd58e153d144e49d1127c5c25bfcc5c532c76f9e43ceea; all four archive sizes and SHA-256 digests match GitHub uploaded-asset metadata. Released means development-channel publication; production was separately inspected at 0.1.0, revision 15123a3ef275458efc87200400219aeacc3e9ea9. No second bump or production rollout is implied.
