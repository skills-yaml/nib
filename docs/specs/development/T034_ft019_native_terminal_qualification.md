# T034: FT-019 Native Terminal Qualification

**Status:** Development

**Related:**
[FT-019: Codex-Inspired Chat and TUI Interactions](ft_019_codex_inspired_chat_and_tui_interactions.md),
[T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T030: Unified Interactive CLI and Plain-Mode Fallback](../done/T030_unified_interactive_cli_and_plain_mode_fallback.md), and
[T033: FT-019 Exact-Run Live Steering](../done/T033_ft019_exact_run_live_steering.md)

## Summary

Qualify FT-019 through the native terminal mechanisms used on Linux, macOS, and
Windows. Keep the release-binary smoke credential-free and Mock-only, drive both a
capable full-screen terminal and the `TERM=dumb`/redirected fallback, and prove that
the host console state is restored after normal exit, child failure, and timeout.

The local implementation makes the Unix smoke portable to hosted macOS and adds a
Windows ConPTY release-binary smoke. Exact macOS and Windows acceptance still requires
the corresponding native CI jobs to execute successfully on the completion revision.

## Scope

- Run the existing optimized-binary interactive smoke on both Linux and macOS, using
  the platform's native `script` pseudo-terminal dialect and portable bounded timeout
  supervision.
- Preserve the isolated HOME/config/project/session setup, Mock provider, ambient
  credential removal, privacy sentinels, hard deadlines, and cleanup assertions.
- Extend the repository-owned Windows `conhost.exe --headless` adapter with bounded,
  timed input chunks so a real interactive child console can be driven deterministically.
- Record console input/output mode snapshots before and after every Windows adapter run
  and fail when the adapter does not restore the modes it changed.
- Add a Windows release-binary smoke that drives the built `nib.exe` through ConPTY,
  covers capable TUI and `TERM=dumb`/plain degradation, and remains offline/Mock-only.
- Expose stable Task entry points and run the native smoke after the optimized build in
  the macOS and Windows CI jobs.
- Add deterministic static contract tests for Task, CI, Unix portability, ConPTY input
  bounds, restoration evidence, and offline/privacy invariants.
- Update technical documentation and reconcile FT-019 without claiming hosted evidence
  that has not run.

## Non-Goals

- Provider credentials, external network calls, paid/live model qualification, or
  production release publication.
- Replacing the existing Ratatui/TestBackend and redirected semantic suites.
- Claiming that cross-compilation or Linux execution proves native macOS/Windows
  terminal behavior.
- Changing FT-019 command, reducer, session, approval, steering, or queue semantics.
- Changing production managed-process authority on Windows or macOS.

## Acceptance Criteria

- [ ] The Unix release-binary smoke runs on native Linux and macOS without GNU-only or
      Bash-4-only dependencies and selects the correct native `script` dialect.
- [ ] Unix capable-terminal and `TERM=dumb`/redirected cases remain bounded,
      credential-free, Mock-only, isolated, privacy-scanned, and restore terminal mode,
      alternate-screen, and bracketed-paste state.
- [ ] The Windows ConPTY adapter accepts only bounded timed input chunks, drains output
      concurrently, preserves the exact child exit status, and keeps timeout cleanup
      bounded with no surviving console descendant.
- [ ] Every Windows adapter invocation records before/after console input/output modes
      and proves restoration on success, child failure, and timeout.
- [ ] The Windows native release-binary smoke drives a capable interactive session and
      a `TERM=dumb`/plain fallback through the real inbox console adapter using only the
      isolated Mock configuration.
- [x] Task exposes documented Unix and Windows binary-smoke entry points, and native
      macOS/Windows CI invokes the matching smoke after building the optimized binary.
- [x] Static contract tests fail on removed platform routing, input bounds, restoration
      evidence, Mock/offline isolation, or CI wiring.
- [ ] Native Linux, macOS, and Windows jobs pass their exact smoke tasks on the same
      clean completion revision.
- [x] Independent spec-compliance and code-quality/security reviews have no unresolved
      blocking findings.
- [ ] `task test:installers`, `task docs:check`, `task check`, `task test`,
      `task check:all-targets`, `task coverage`, `task build`,
      `task smoke:interactive`, the native Windows binary smoke, and
      `git diff --check` pass on the completion revision.

## Affected Areas

- `scripts/check-interactive-release.sh` — portable Linux/macOS PTY and timeout logic.
- `scripts/invoke-windows-pseudoterminal.ps1`,
  `scripts/test-windows-pseudoterminal.ps1`,
  `scripts/host-windows-pseudoterminal.ps1`, and
  `scripts/start-windows-pseudoterminal-child.ps1` — bounded ConPTY input and console
  restoration evidence.
- `scripts/check-interactive-release.ps1` — isolated Windows release-binary smoke.
- `src/console.rs` — lazy bounded stdin broker for unattended one-shot execution.
- `Taskfile.yml` — stable native smoke entry points and focused console coverage.
- `.github/workflows/ci.yml` — post-build macOS and Windows native qualification.
- `tests/installers.rs` — deterministic script, Task, and workflow contract tests.
- `docs/tech/task.md`, `docs/tech/ci.md`, and the FT-019 reconciliation.

## Implementation Plan

1. Port the Unix smoke to native Darwin while retaining Linux behavior, bounded
   supervision, isolation, privacy checks, and restoration assertions.
2. Add bounded input-chunk delivery and console-mode before/after evidence to the
   Windows headless-console adapter; extend its existing success/failure/timeout probe.
3. Add the isolated Mock-only Windows `nib.exe` smoke and stable Task commands.
4. Wire macOS and Windows CI after their release builds and add static contract tests.
5. Update technical docs, run focused and canonical gates, complete two-stage review,
   then reconcile hosted native evidence before moving T034 or FT-019 to Done.

## Local Linux Qualification (2026-09-02)

`task smoke:interactive` passed the deterministic steering, interaction, TUI, console,
chat, redirected-CLI, and installer contract suites, then built the locked optimized
binary and passed the offline Linux PTY and redirected-mode smoke with terminal-state
restoration. `task test:installers` passed 40/40, `task verify` passed the complete
serial suite, host and Windows MSVC all-target checks passed, runtime line coverage was
85.87 percent (101,945/118,726), and `git diff --check` passed. These results prove the
Linux/local portions only; native macOS and Windows terminal behavior and the same clean
revision hosted matrix remain open.

## Validation Gates

1. `task test:windows-pseudoterminal` proves exact status propagation, bounded input,
   concurrent drain, timeout process-tree cleanup, and restoration evidence on a native
   Windows host.
2. `task smoke:interactive:binary` runs the already-built optimized binary through the
   native Linux/macOS pseudo-terminal and redirected fallback.
3. `task smoke:interactive:windows:binary` runs the already-built optimized `nib.exe`
   through native ConPTY and the redirected/`TERM=dumb` fallback.
4. `task test:installers` validates script syntax and the static Task/CI/offline/privacy
   contracts without needing a native terminal or network.
5. The canonical documentation, Rust, coverage, optimized-build, interactive-smoke,
   exact-revision, and diff gates listed in the acceptance criteria pass.

## Risks and Mitigations

- **Pseudo-terminal dialect drift:** Select `script` invocation by `uname` and keep
  static contract coverage for both Linux and Darwin forms.
- **Unbounded child or blocked pipe:** Bound input count/bytes/delays, drain both pipes
  asynchronously, enforce an internal deadline, and kill the complete host tree.
- **Terminal damage after failure:** Snapshot only valid console handles, restore every
  mode changed by the adapter in `finally`, and emit machine-checkable before/after
  evidence.
- **False native proof:** Keep T034 in development until exact hosted native artifacts
  exist; do not treat cross-target compilation as terminal execution evidence.
- **Credential or network leakage:** Remove provider credentials, configure only Mock,
  isolate state, retain inactive-provider sentinels, and scan all captured output and
  ledgers before accepting a run.

## Implementation Reconciliation (2026-08-27)

The locally actionable harness work is implemented. The Unix release smoke now selects
GNU/Linux or BSD/macOS `script` syntax, avoids Bash-4/GNU-only traversal and path tools,
uses a native Darwin watchdog, and exercises `/status` before `/quit` in `TERM=dumb`.
The Windows adapter accepts bounded delayed input, emits unforgeable compact child-mode
evidence, compares caller modes even on timeout, and preserves its existing exact-exit
and resistant-descendant cleanup contracts. A new Mock-only `nib.exe` smoke exercises
capable TUI restoration and plain fallback, and native CI runs the matching smoke after
each macOS/Windows optimized build.

Local evidence passed `task installers:check`, all 39 `task test:installers` tests,
`task docs:check`, `task smoke:interactive:binary` on Linux, and `git diff --check`.
T034 remains in development: the exact native macOS and Windows jobs have not run on
this dirty local revision, the canonical aggregate gates remain the root integration
owner's responsibility, and the same clean completion revision is not available.

Independent implementation-scope spec review and a separate code-quality/security/
portability review both passed with no unresolved findings. The latter review first
identified and then verified repairs for scanning every persisted Unix session ledger
for the inactive-provider sentinel and for failing Windows smoke cleanup when the
exact isolated fixture remains. Static installer contracts protect both boundaries.

The final Linux native run exposed a first-use terminal boundary that redirected-only
tests could not reproduce: the one-shot `nib run --yes` path eagerly spawned its stdin
broker, and GNU `timeout` correctly placed the child in a background process group,
where the unused terminal read stopped it with `SIGTTIN`. `ConsoleInput` now keeps its
bounded reader dormant and starts the broker exactly once on the first actual approval,
question, or direct read. `task test:interactive` now includes all six console tests so
this boundary is part of the focused 160-test gate. The unchanged 60-second watchdog
then passed `task smoke:interactive` end to end on native Linux, including the optimized
build, PTY/redirected cases, modal framing, privacy scans, and terminal restoration.
Final local canonical evidence also passed `task test` (943 library tests, 86 binary
tests, and every integration/doctest target), `task test:installers` (39/39),
`task docs:check` (5/5), `task check`, `task check:all-targets`, `task coverage` at
85.98% (85,650 / 99,615), and `task build`. These dirty-worktree Linux results do not
replace the still-open same-clean-revision macOS/Windows/native aggregate evidence.
