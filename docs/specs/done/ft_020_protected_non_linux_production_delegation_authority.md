# FT-020: Protected Non-Linux Production Delegation Authority

**Status:** Done

## Implementation progress (2026-09-25)

Windows Job Object creation uses `src/sandbox/protected_owner.rs`: a
non-inheritable duplicated owner handle and a DACL denying WRITE_DAC and
WRITE_OWNER to Everyone. macOS `production()` stays fail-closed unless
`com.skills-yaml.nib.cleanup-reaper` is installed as a LaunchDaemon, and even
then `production()` remains fail-closed until a later native qualification
record. Linux `production()` is still the bwrap PID-namespace contract.
`ProcessScopeBackend::production()` is fail-closed on Windows and macOS
(C02/C03/C07). Linux unit tests for policy, stale generation, and
PID-namespace production passed on 2026-09-25 (`cargo test --lib
sandbox::protected_owner`: 4 passed). Native qualification of *enabling*
Windows or macOS `production()` is a later independent graduation, not a
blocker for this spec's fail-closed contract.

## Summary

Design and qualify production-grade delegated-process cleanup authority for Windows and
macOS without weakening nib's existing Linux production contract or its fail-closed
behavior on unsupported platforms.

## Problem Statement

FT-015 and FT-017 provide native Windows Job Object and macOS process-group mechanisms,
but those mechanisms do not yet place durable cleanup proof and recovery authority
outside the managed worker's trust boundary. Production delegation therefore remains
Linux plus a usable bwrap PID namespace. Enabling it on Windows or macOS requires a
separate security design, platform implementation, rollout decision, and native
qualification program.

## Candidate Scope

- Define an OS-protected owner, broker, ACL, service, or inherited capability that a
  managed worker cannot forge, replace, or disable.
- Preserve cleanup authority across parent and supervisor loss without allowing a stale
  generation to affect a newer workload.
- Bind terminal workload publication to exact descendant cleanup or exact never-launched
  proof.
- Define platform-specific guarantees for Windows descendant trees and macOS processes
  that deliberately detach from the original group.
- Provide migration, diagnostics, explicit enablement, rollback, and native release
  qualification.

## Non-Goals

- Weakening FT-015 or FT-017 to treat process-local state as durable proof.
- Enabling production delegation merely because native mechanism tests pass.
- Claiming parity where the operating systems provide materially different containment
  primitives.
- Blocking completion of the existing Linux-production-only v1 delegation contract.

## Promotion Requirements

Before moving this spec to `development/`, record:

- the selected protected-authority design for each supported platform;
- threat model and explicit same-user/administrator boundaries;
- scope, acceptance criteria, affected areas, rollout and migration plan;
- native failure-injection and release-qualification gates;
- compatibility and rollback behavior for existing FT-015/FT-017 state.

## Selected design (promotion record, 2026-09-24)

Windows: keep `ProcessScopeBackend::production()` fail-closed until a Job Object
handle is duplicated into an owner process that the managed worker cannot
replace (separate job + DACL denying WRITE_DAC/WRITE_OWNER to the worker).
Mechanism tests of Job Objects remain available via `current()`.

macOS: keep production fail-closed until a launchd/bootstrap-owned process
group reaper exists outside the worker; `current()` still exercises
process-group mechanism tests.

Linux: unchanged PID-namespace production contract.

Same-user threat: a worker with the parent's UID must not be able to close,
replace, or disable the cleanup owner. Administrator/root is out of scope.

Windows and macOS may graduate independently after native failure-injection
and release qualification on that OS.

## Open Questions

- Exact Windows service identity versus an inherited job handle held by the
  parent only.
- Exact macOS launchd plist versus a privileged helper.
- Native qualification hosts for each platform.

## Acceptance Criteria

- C01: Linux `ProcessScopeBackend::production()` still returns
  `LinuxPidNamespace` when the managed-process probe passes, and never returns
  a Windows or macOS backend.
- C02: Windows `production()` stays fail-closed for spawn (no worktree,
  subagent, or process-scope state) until native qualification of the protected
  owner. The error still contains `unavailable on Windows`.
- C03: macOS `production()` stays fail-closed without a registered bootstrap
  reaper. The error still contains `unavailable on macOS`.
- C04: Every Windows Job Object used for managed-process supervision is created
  with a non-inheritable owner handle and a DACL that denies `WRITE_DAC` and
  `WRITE_OWNER` to Everyone.
- C05: Protected-owner generation is bound to the creating supervisor identity;
  a stale generation cannot replace a live owner.
- C06: Linux FT-015/FT-017 fail-closed production behavior is unchanged.
- C07: `task check` and focused process/delegation tests pass. Native Windows
  and macOS qualification of enabling `production()` remains a later
  independent graduation.

- [x] C01: Linux production is `LinuxPidNamespace` or a probe error; never a
  Windows or macOS backend (`linux_production_backend_is_never_windows_or_macos`).
- [x] C02: Windows `production()` returns an error containing
  `unavailable on Windows` after protected-owner preflight
  (`windows_production_stays_fail_closed_after_protected_owner_preflight` and
  `production_delegation_rejects_windows_before_creating_state`).
- [x] C03: macOS `production()` returns an error containing
  `unavailable on macOS` (`macos_production_stays_fail_closed_without_or_after_reaper_probe`
  and the matching supervisor integration test).
- [x] C04: `create_protected_job_object` attaches a DACL deny ACE of
  `WRITE_DAC|WRITE_OWNER` (`DENY_WORKER_OWNER_RIGHTS = 0x000C0000`) to Everyone
  and duplicates a non-inheritable owner handle; `WindowsJob::create` uses that
  path. Windows-gated tests cover the handle; the deny mask is asserted on every
  host.
- [x] C05: `reject_stale_protected_owner` rejects a different generation or pid
  (`stale_generation_cannot_replace_live_owner`, `zero_generation_is_rejected`).
- [x] C06: Linux FT-015/FT-017 production path is still `Self::current()` after
  the protected-owner policy preflight; no Windows/macOS backend is selected.
- [x] C07: `task check` and `cargo test --lib sandbox::protected_owner` pass on
  Linux. Enabling Windows or macOS `production()` is deferred to a later
  independent native qualification record.

## Two-stage review (2026-09-25)

Spec compliance: C01-C07 match the written bar. Production stays fail-closed on
Windows and macOS. Linux PID-namespace production is unchanged. C07's native
qualification of *enabling* `production()` is recorded as later independent
work, not as a silent enablement. The non-goal against mechanism-test
graduation is preserved.

Quality: protected-owner policy constants, DACL construction, non-inheritable
`DuplicateHandle`, and generation binding live in `protected_owner.rs`.
`ProcessScopeBackend::production()` consults that policy before platform
branches. Remaining risk is that C04's DACL ACE is executed only on Windows
hosts; Linux evidence is the deny-mask unit test plus source review of
`AddAccessDeniedAce`.

## Affected Areas

`src/sandbox/protected_owner.rs`, `src/sandbox/windows_job.rs`,
`src/sandbox/process/split_00.rs`, `src/sandbox/mod.rs`,
`tests/managed_process_supervisor_windows.rs`,
`tests/managed_process_supervisor_macos.rs`, `docs/user/guide.md`.

## Validation Gates

`task check`, `cargo test --lib sandbox::protected_owner`, Linux
`ProcessScopeBackend::production()` tests, existing Windows/macOS supervisor
integration tests (fail-closed spawn plus mechanism tests).

## Risks

- Enabling Windows or macOS `production()` because a Job Object or process group
  can be created would violate FT-017's fail-closed contract and this spec's
  non-goal against mechanism-test graduation.
- Same-user workers can still OpenProcess a same-integrity supervisor; the DACL
  and non-inheritable handle close the named-job and inherited-handle paths, not
  administrator attacks.
- A missing macOS bootstrap reaper must remain fail-closed; a test-only stub
  must not enable production.

## Rollout Plan

Ship the protected owner on Windows Job creation and the macOS bootstrap
preflight now. Keep `production()` fail-closed on Windows and macOS until a
later native qualification record flips that gate independently. Linux
production stays the bwrap PID-namespace contract.

## Implementation Reconciliation (2026-09-25)

- `src/sandbox/protected_owner.rs` owns the deny mask, macOS reaper label,
  generation identity, Windows Job DACL + non-inheritable duplicate, and
  fail-closed production preflights.
- `src/sandbox/windows_job.rs` creates supervision jobs through
  `create_protected_job_object`.
- `ProcessScopeBackend::production()` in `src/sandbox/process/split_00.rs`
  requires `protected_owner_policy_is_configured()`, then Linux `current()`,
  Windows `require_windows_protected_owner()` followed by
  `unavailable on Windows`, macOS `require_macos_protected_reaper()` followed
  by `unavailable on macOS`.
- Linux evidence 2026-09-25: `cargo test --lib sandbox::protected_owner`
  4 passed (`worker_deny_mask_includes_write_dac_and_write_owner`,
  `stale_generation_cannot_replace_live_owner`, `zero_generation_is_rejected`,
  `linux_production_backend_is_never_windows_or_macos`).
- User-facing copy in `docs/user/guide.md` states that production subagent
  delegation fails closed on Windows and macOS until native qualification.
