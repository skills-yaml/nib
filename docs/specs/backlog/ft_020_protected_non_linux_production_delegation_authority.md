# FT-020: Protected Non-Linux Production Delegation Authority

Status: Backlog

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

## Open Questions

- Which Windows service, ACL boundary, or inherited handle owns cleanup proof outside
  both the nib parent and managed worker?
- Which independent macOS owner can recover a crashed supervisor and what guarantee is
  possible for a deliberately detached descendant?
- Should Windows and macOS graduate independently when only one platform has a proven
  production authority?
