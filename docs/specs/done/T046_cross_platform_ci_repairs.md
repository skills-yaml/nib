# T046: Cross-platform CI repairs

**Status:** Done

Implementation, local verification, and exact-revision hosted Linux, macOS, and
Windows acceptance are complete. Final evidence is recorded below.

## Problem

CI run 35339709145 for development commit 77aa781 failed Linux static checks,
seven Windows library tests, and the macOS native interactive smoke. Release
Artifacts succeeded independently, so its success does not prove CI acceptance.

## Goals and scope

Repair the redundant context-test formatting, Windows instruction scope resolution,
legacy reconciliation failure, terminal restoration test portability, and native
interactive smoke regressions, including child-session fixture handles retained across
worktree cleanup and bounded, prompt-synchronized Windows terminal input. Preserve
bounded instruction discovery, link rejection,
auditable workload reconciliation, explicit workspace consent, terminal restoration,
and credential-free native qualification.
Restore Windows mouse capture before disabling raw mode so mouse cleanup cannot
reapply the raw input mode it saved during TUI initialization.

Affected areas: `src/context/`, `src/tools/delegation.rs`, `src/agent/loop.rs` test
fixtures, `src/tui/`, interactive
smoke scripts, focused regression tests, and Task entries only where needed.
No persistence schema changes, release publication changes, or new frameworks.

## Design and implementation plan

1. Remove the redundant formatting expression without changing test data.
2. Resolve valid Windows root aliases without weakening worktree containment or
   accepting linked paths; cover existing and prospective child paths.
3. Diagnose the legacy-audit reconciliation deadline failure and fix the demonstrated
   cause while retaining bounded locking and exactly-once audit adoption.
4. Separate pure terminal-sequence assertions from native console operations where
   necessary, retaining native Windows restoration verification. Defer raw-mode
   restoration until mouse, paste, and alternate-screen cleanup have all been
   attempted; preserve error aggregation and restore-guard behavior.
5. Update native smoke input and assertions for current consent and interaction
   behavior. Preserve exact child status, bounded waits, persisted-state assertions,
   privacy checks, and terminal mode restoration.
   Add optional output-prompt gates to the existing Windows input chunks, keeping
   asynchronous output drains and the same absolute child timeout. Verify prompt
   matching across output chunks and bounded missing-prompt failure before using
   separate consent, quit-arm, and quit-confirmation writes in native smoke.
6. Review spec compliance, then code quality; run local gates and record native
   platform limitations explicitly.
7. Align the two hosted steering tool-readiness fixtures with the existing bounded
   fifteen-second hosted progress budget. Keep event and persisted-state ordering
   assertions, fail immediately if the stream closes before tool readiness, and
   leave every production deadline unchanged.

## Alternatives

Disabling warnings, skipping native tests, ignoring pipeline failures, and removing
consent would hide regressions. Reuse the existing filesystem and terminal abstractions
and repair fixtures or implementation at the demonstrated failing boundary.

## Acceptance criteria

- [x] Linux static checks pass with no new lint suppressions.
- [x] Valid Windows instruction scopes resolve while outside-root and linked paths fail.
- [x] Legacy audit adoption retains one reconciliation event without lock failure.
- [x] Native Windows merge verification preserves results and removes the owned
      worktree after test observers release child-session handles.
- [x] Terminal restoration tests work without assuming a Windows console in unit tests.
- [x] Raw-mode cleanup runs after mouse restoration even when another restoration
      step fails, and native before/after console mode checks remain exact.
- [x] Windows terminal input waits for actual consent and quit prompts within the
      original deadline; missing prompts fail boundedly without sending input.
- [x] Steering tool-readiness fixtures allow bounded hosted filesystem startup and
      fail immediately on early stream closure while preserving steering order checks.
- [x] Offline interactive smokes exercise current consent and interaction behavior,
      validate successful child exit and exact terminal restoration, and remain bounded.
- [x] `task verify`, documentation checks, relevant focused checks, runtime coverage,
      and Linux release interactive smoke pass locally.
- [x] Exact-revision hosted Windows and macOS CI pass before native closure.

## Validation gates

Use `task check`, `task test:agent-context`, `task test:interactive`, relevant
delegation regression tasks, `task verify`, `task docs:check`, `task coverage`, and
`task smoke:interactive`, `task test:windows-pseudoterminal-output` where PowerShell
is available, and native `task test:windows-pseudoterminal`. Cross-check Windows
compilation if the target is installed.
Hosted CI remains authoritative for native Windows and macOS execution.

## Risks and rollout

Path normalization is a containment boundary: retain component and file identity
validation. Reconciliation changes must preserve lock ordering and audit identity.
Smoke fixtures must test consent rather than globally bypassing it. Ship as a focused
repair; exact native acceptance is recorded below.

## Open questions

None for this repair. Native acceptance is complete; credentialed live LLM
qualification remains separately scoped by T023.

## Implementation findings (2026-09-19)

- Separate hosted Windows runs exhausted the five-second tool-readiness waits in
  `exact_run_steering_stays_closed_when_the_final_provider_turn_starts_a_tool` and
  `exact_run_steering_after_tool_start_applies_before_the_next_provider_request`.
  Their event gates include runtime startup and durable session I/O, and the latter
  also waits for persisted tool-start evidence. Use the adjacent compression fixture's
  fifteen-second hosted progress budget; do not change the production run limits.
  A closed stream must fail explicitly instead of spinning until the timeout.
- Instruction discovery canonicalized the root but compared it lexically against
  caller scopes containing Windows verbatim-prefix or DOS-short-path aliases. Match
  only the root alias through the existing filesystem helper and retain child path
  spelling for containment and link checks.
- The legacy audit fixture's two-second deadline covered filesystem work and final
  identity readback, not just lock acquisition. The preceding hosted Windows run
  passed it in approximately 0.82 seconds. Align the fixture with the existing
  five-second production record-lock budget; retain production deadlines and test
  retry idempotency plus the existing contention/deadline regression.
- Crossterm can call native Windows console APIs even with an in-memory output
  writer. Pure ANSI encoding tests must not assume a live native console.
- The automatic TUI smoke sent two quit keystrokes. At the new workspace-consent
  prompt, the first immediately declined and exited, leaving the second write to
  fail with SIGPIPE on macOS. The smoke must explicitly accept and verify consent
  before exercising the rest of the interaction. Normal TUI quit still requires
  two keystrokes; Unix can emit them together once consent has been persisted.
- Plans now print and continue automatically. Replace obsolete plan-approval
  expectations with an isolated instruction policy requiring approval for
  `list_directory`, and verify question answers and action approval in persisted
  session evidence. No product permission behavior changes are needed.
- Repair PR run 35442785518 passed the original Windows library failures, then
  exposed a delegation integration fixture retaining a child `SessionStore` across
  worktree cleanup. Its open identity file prevents Windows from renaming the
  ancestor directory into quarantine. Scope the readback before merge; preserve
  production identity fences and every verification, result, and cleanup assertion.
  macOS passed its complete native job on that run.
- The same repair run reached the Linux TUI approval dock, but its prompt wait
  searched for the full title in incremental terminal bytes. Ratatui reused the
  previous frame's `s` cell and emitted `Li` followed by a cursor move and
  `t this directory`, so the visible title never appeared as a contiguous byte
  sequence. Wait for the fresh `Approve once (y)` choice row instead; retain
  persisted user-approval and completed-run evidence, bounded waits, and exact
  terminal restoration checks.
- A later hosted Windows run passed library/integration tests and optimized release
  qualification but timed out in the native smoke. Input delays started at conhost
  launch, before child PowerShell initialization and the consent prompt. The adapter
  also hid the inner failure behind its outer timeout. Synchronize input against
  actual output and preserve bounded inner diagnostics. Adjacent identical Windows
  key events can be coalesced, while Crossterm's Windows parser ignores repeat count;
  separate quit writes around the visible confirmation prompt. The captured timeout
  alone does not establish which input hazard occurred on that runner.
- Once prompt synchronization let the Windows child exit, its mode check exposed
  a production restoration-order defect: `disable_raw_mode()` ran eagerly before
  `DisableMouseCapture`. Crossterm's Windows mouse cleanup restores the input mode
  captured after raw mode was enabled, thereby re-enabling raw input. Defer raw
  cleanup until the other restoration steps finish. Keep exact native comparisons
  and add nonsecret before/after mode tokens to diagnostics for remaining failures.

## Validation evidence (2026-09-19)

- The portable `task test:windows-pseudoterminal-output` passed using PowerShell
  7.6.6 on Linux. It parsed all affected adapters, compiled the asynchronous capture,
  matched a live split prompt before EOF, preserved complete output, enforced a
  missing-prompt deadline, rejected a post-prompt delay exceeding the remaining
  absolute budget, and covered final-append/EOF ordering. This does not
  substitute for native ConPTY success, missing-prompt, and restoration probes.
- `task verify` passed strict formatting and all-target/all-feature Clippy, all 1,161
  library tests, all 86 CLI tests, every deterministic integration target, and
  doctests. Credentialed live qualification and the separately invoked exact-release
  harness remained explicitly ignored as designed.
- `task test:agent-context` passed 59 context, 73 agent/planner, and two build-metadata
  tests, including the existing long-instruction fixture and new containment/link
  regressions.
- `task test:delegation` passed the legacy-audit adoption/retry and held-modern-stripe
  single-deadline regressions, supporting persistence checks, all 37 managed-process
  tests, and all 22 delegation integration tests.
- Independent spec-compliance and context containment code-quality reviews found no
  blocking findings. Final independent terminal-smoke review likewise found no
  weakened success, approval, privacy, or restoration checks. These reviews do not
  substitute for native execution.
- `task docs:check` passed all five checks; `task installers:check` and
  `git diff --check` passed.
- `task build` and `task smoke:interactive:binary` passed. The full offline Linux
  PTY/redirected smoke also passed against the debug binary while the optimized
  build was being prepared. It exercised consent, action approval, questions,
  composer/history, queued work, steering, cancellation, resume, and privacy with
  successful child status and exact terminal restoration.
- `CARGO_BUILD_JOBS=2 task coverage` passed its complete instrumented suite and
  reported **85.84% runtime line coverage (112,201/130,706 lines)**, above the
  configured 80% gate. Reduced build parallelism bounded compiler memory use.
- Windows cross-target checking was attempted through
  `task check:all-targets TARGET=x86_64-pc-windows-msvc`, but failed in dependency
  compilation because the local MSVC librarian `lib.exe` is unavailable, before
  checking nib. The final hosted Windows run below supersedes this local limitation.

Local smoke evidence is complemented by the final hosted Windows alias/junction,
console-free encoding, and native interaction checks recorded below. No production permission policy,
persistence schema, or delegation timeout behavior was changed.
Production terminal restoration now disables raw input after Windows mouse cleanup.

## Incremental terminal prompt follow-up (2026-09-19)

A bounded temporary Task probe drove the existing optimized Linux binary at the
Windows smoke's 120-column, 30-row geometry with `NO_COLOR=1`. Consent succeeded,
but the raw output split `Allowed work in this directory.` with cursor-position
escapes before `in`, `this`, and `directory.`. A second probe reached the first quit
request and likewise split `Press Ctrl+Q again to quit.` between every word;
`Ctrl+Q again` was not contiguous either. Complete visible sentences therefore
cannot be used as raw-stream gates for these incremental redraws.

Use the observed contiguous `Allowed work` and `again` segments, which are unique
to consent success and quit confirmation in this isolated startup. A third probe
completed both separately gated quit writes and passed persisted consent, exact
child exit status, bracketed-paste/alternate-screen restoration, and exact terminal
mode restoration. The tracked Windows smoke retains all those assertions and its
absolute deadline. This Linux evidence establishes the raw-output failure and
repair mechanics; the final hosted run below also proves native ConPTY acceptance.

## Final acceptance (2026-09-19)

[CI run 35451287933](https://github.com/skills-yaml/nib/actions/runs/35451287933)
passed every Linux, macOS, and Windows job for PR head
`f7d5bf91dd9ea4ac24b4067b7b5f79059350ac18`, tested through merge revision
`b2f081c83a48dcd118b91703214a8ddc76b11dd9`.

- Linux passed strict static checks, 1,162 library tests, 86 CLI tests, all
  integration gates, **85.91% runtime line coverage (112,331/130,753)**,
  exact release qualification, interactive smoke, and managed-process owner loss.
- macOS passed its complete native suite, exact release qualification, and PTY
  interactive/restoration smoke.
- Windows passed 1,072 library tests, 91 CLI tests, all integration gates,
  bounded prompt/timeout probes, exact release qualification, ConPTY and plain
  interaction with exact caller/child console restoration, and binary smoke.
- Final local `task verify`, `task test:interactive`, installer/documentation
  checks, portable PowerShell output checks, a fresh `task build`, and optimized
  `task smoke:interactive:binary` passed. The focused TUI suite includes the new
  cleanup ordering and combined-error regression.
- Independent spec-compliance and code-quality reviews passed after correcting
  the prompt EOF race and prompt-plus-delay deadline gap found during review.

Earlier failed runs remain diagnostic history, not acceptance evidence. An isolated
Linux delegated-result assertion also failed on an intermediate run without enough
error detail to establish a cause; final local, hosted, and coverage suites passed
it without weakening its assertions. No production permission policy, persistence
schema, or delegation/steering deadline was changed.

## T047 supersession (2026-09-21)

[T047](../development/T047_user_interaction_harmonization.md) retains T046's native
prompt, deadline, and terminal-restoration mechanics while changing approval defaults,
question recovery, Ctrl+C/Ctrl+Q semantics, one-shot interruption, and clipboard
reporting. T046's completed native evidence remains valid for its exact historical
revision only; T047 must rerun the Linux/macOS/Windows smoke matrix against its own
exact binaries before completion.
