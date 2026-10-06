---
schema_version: 1
coordination_id: t061-plain-agent-worker
agent_id: t061-plain-worker
role: implementer
status: active
base_revision: 535059285c56791c279afda4fb386c9cc816eed8
task_ref: detached
branch_authorization: none
updated_at: 2026-10-06T05:05:36Z
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
assertions before reaching the low-stack CLI regression. Corrected-source Task
fmt passed; strict check, interaction, full tests (including the colocated
ownership test) and documentation gates remain pending. Renewed independent
exact source review is pending.
The shared applied nib-question-form minor reservation remains 0.3.0 from
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

Format and checkpoint the source for independent review, then run the affected
author gates and publish a clean exact handoff for registered integration.

## Handoff

Pending.
