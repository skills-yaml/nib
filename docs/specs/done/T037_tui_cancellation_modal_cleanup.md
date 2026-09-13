# T037: TUI Cancellation Modal Cleanup

**Status:** Done

**Related:** [T031](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T036](../done/T036_conversational_tui_visual_hierarchy.md)

## Problem

Hosted macOS qualification reaches the edited composer draft and its cancelled
terminal record, but the next path-completed draft is not submitted. Inspection
identified a race, confirmed by a deterministic regression: shutdown drains approval/
question requests before joining the worker, while a request published during
shutdown can remain queued afterward.
The next input loop can then reopen a stale modal and discard a new paste. A
regression failed on the old path and passes after rejecting late requests following
the join. The unavailable hosted terminal capture does not by itself prove that this
is the macOS failure's cause.

## Scope and Affected Areas

- `src/tui/mod.rs`: ensure a successfully joined cancelled worker leaves no queued
  approval/question requests or active modal capable of consuming later input.
- Deterministic unit tests in that module: publish requests after initial cancellation
  cleanup and verify their disposition before the next interaction.
- `scripts/check-interactive-release.sh`: retain bounded, credential-free failure
  diagnostics from its isolated fixture so future native failures are diagnosable.
- `Taskfile.yml` and `docs/tech/task.md`: expose the focused shutdown regression gate.
- This spec, the T036 follow-up evidence, and `docs/specs/README.md`.

No approval policy, run persistence schema, provider contract, cancellation deadline,
or production delegation authority changes are in scope.

## Acceptance Criteria

- [x] A deterministic regression demonstrates late modal requests surviving the old
      shutdown path and passes after the repair.
- [x] Successful cancellation joins the worker, rejects pending and late approval/
      question requests, and returns to a usable composer without stale modal state.
- [x] Cancellation and timeout behavior retain existing reconciliation, queue,
      authority, and terminal restoration guarantees.
- [x] Native smoke failures expose bounded diagnostic evidence from only the isolated
      Mock fixture; successful runs preserve existing privacy checks and cleanup.
- [x] Spec then quality review, focused tests, `task verify`, documentation checks,
      and optimized native smoke pass before completion.

Fresh Linux/macOS/Windows CI is an additional merge gate. Its exact committed-revision
results are recorded in PR #25 before merge.

## Implementation Plan

1. Reproduce the late-request race with synchronization rather than timing sleeps.
2. Drain and reject late modal requests after the producer worker has joined.
3. Add bounded native failure diagnostics without changing success assertions.
4. Review spec compliance then code quality; run focused and complete local gates.
5. Record evidence and qualify the exact committed head on all native CI runners.

## Validation Gates

- `task test:interactive` and `task test:tui-shutdown`.
- `task verify`, `task docs:check`, and `git diff --check`.
- `task build` followed by `task smoke:interactive:binary`.
- Fresh PR CI on Linux, macOS, and Windows, including native release qualification.

## Risks

Cleaning up before worker termination alone cannot establish that its request stream
is finished. Cleanup must follow a successful join while retaining early cleanup to
release blocked worker dependencies. Timeout still exits the TUI rather than allowing
an unjoined producer to interact with a subsequent turn. Diagnostics must never read
the user's real session, configuration, or provider environment.

## Initial Regression Evidence (2026-09-12)

`task test:tui-shutdown` first passed the two existing cancellation/timeout tests and
failed the new regression because a cancelled approval reopened. A full stream
channel and an initial question reply synchronize publication of both late modal
requests after the first cleanup; the test does not depend on timing sleeps.

Repeating modal cleanup after the successful worker join makes all three tests pass.
The regression also verifies explicit approval denial, question cancellation, stream
reconciliation, and return to composer routing. The early cleanup and timeout path
remain intact.

## Completion Evidence (2026-09-12)

Independent spec-compliance and code-quality reviews passed. `task test:interactive`
and `task verify` passed, including 1,419 tests (1,078 library, 86 CLI, and 255
integration tests), formatting, installer syntax, and warning-denying Clippy.
The locked optimized build passed, followed by the complete offline Linux PTY and
redirected-mode `task smoke:interactive:binary` against the repaired binary.

A deliberate missing-history assertion exercised the failure path through the native
smoke. It emitted bounded escaped terminal and composer-ledger diagnostics without raw
terminal controls or the fixture credential sentinel. The smoke source was restored
exactly afterward, and the outer script removed its failed fixture. The subsequent
successful smoke retained the normal privacy and terminal-restoration assertions.
`task docs:check` and `git diff --check` also passed after the lifecycle update.

The prior hosted commit passed Linux and Windows validation, with only the macOS
composer smoke failing. Fresh hosted validation of this repair remains a required
merge gate, with exact-revision evidence recorded in PR #25. T023's live-provider
qualification and FT-020's production delegation authority remain outside this fix.
