---
schema_version: 1
coordination_id: t061-plain-agent-worker
agent_id: t061-plain-worker
role: implementer
status: complete
base_revision: 535059285c56791c279afda4fb386c9cc816eed8
task_ref: detached
branch_authorization: none
updated_at: 2026-10-06T05:39:42Z
scope:
  - src/chat/split_00.rs
  - src/chat/split_01.rs
  - tests/interactive_cli.rs
---

# Peer Work Record

## Assignment

Repair T061/T067 plain-chat native debug execution at the established agent
runtime worker boundary, supporting AC-3, AC-5 and AC-9.

## Actions

Created an isolated worktree through an atomic peer claim from the integrated
HTTP and native prompt repairs. The confirmed native Windows failure is a real
main-thread stack overflow before question admission, not a modal timeout.

First changed only the existing live-Esc Unix launcher to give the real child
a 1 MiB main stack through positional shell arguments. Kept Windows direct
launch, both Esc inputs, all 15-second deadlines and every persisted-state
assertion. The unfixed Task interaction gate reproduced SIGABRT with main-thread
stack overflow; the final persisted event is tool_proposal_commit, matching the
native Windows failure before question_required. Twelve other CLI cases and
all preceding interaction modules passed.

Use the established build_agent_runtime configuration. Move owned project,
profile, session directory, session id, goal and unchanged loop configuration
into a spawned worker, constructing the boxed agent future inside that worker.
The caller retains its input/modal loop and selects only the worker JoinHandle.
Join errors return static cancellation/panic messages; completion still clears
modal responders, awaits exact reconciliation and joins the stream renderer.
No production stack setting, environment stack override, deadline, native
exclusion, approval policy or recovery admission change is introduced.

Independent review found that an unowned JoinHandle could detach the worker
on router unwind and retain the stream sender while renderer cleanup waits.
A private abort-on-drop owner now cancels it before prepared-state cleanup.
A colocated bounded regression unwinds that owner while a real worker holds
modal/stream resources, then checks cancellation, resource closure and absence
of dependent continuation.

The first positive interaction run exposed two unchanged goal/session-switch
assertions: buffered input could cancel the worker before its first real poll.
A first-poll handshake now restores the former biased polling order. Only a
Pending first poll enables input routing; Ready or worker failure joins before
routing buffered input. No goal is pre-appended or duplicated.

## Validation

Negative task test:interactive reproduced the real stack failure on the unfixed
code. Initial positive interaction validation failed those two existing chat
assertions before reaching the low-stack CLI regression. Strict check caught
the test-module placement rule; moving the unchanged module after production
items resolved it without a lint allowance.

Corrected-source Task fmt and strict check passed. Task test:interactive passed
all 302 cases, including both low-main-stack Esc inputs and the unchanged
goal/session-switch assertions. Full Task test passed 1,335 library and 106 CLI
unit tests, including the private worker-unwind regression; all remaining
integration suites and doctests passed, including 54 runtime end-to-end cases.
The two opt-in live/release qualification cases remain ignored by the ordinary
suite; no paid live call was made. Strict check and documentation validation (five integrity cases) were
renewed successfully after freezing these records.

Independent two-stage review of source checkpoint 4162fa7 resolved the detached
worker and first-poll ordering findings with no remaining confirmed defect.
The only subsequent source change moves the identical test module after
production items. Final exact handoff review, combined native integration and
hosted Windows qualification remain pending; no delivery is claimed.
The shared applied nib-question-form minor reservation was freshly rechecked
after aggregate validation and remains 0.3.0 from
0.2.0; no second bump. Cargo runs serially through Task with one build job,
debug symbols disabled and the configured native bwrap support flag.

## Blockers and Dependencies

None.

## Memory Impact

Status: none
Rationale: This bounded repair applies the established runtime worker
architecture to plain chat without a new durable product decision. Shared
delivery reconciliation remains pending.

## Next Step

Publish the clean exact handoff and release the serialized Cargo slot for
independent final review and registered integration.

## Handoff

Source, focused/aggregate author gates and final record gates passed. This
committed candidate is ready for clean exact handoff and independent approval;
shared integration and native Windows qualification remain separate delivery
gates.

Peer integration contains reviewed source revision `e52c761a441a1ec2b6d045532f592a7c550c854e`. The internal integration ref advances only after native gates pass; this is not evidence of test or production release.
