---
schema_version: 1
coordination_id: t061-native-http-fixture
agent_id: t061-http
role: implementer
status: active
base_revision: d8b8d82fa55fb7f5f5f32ae1da87a1ccf157ea88
task_ref: detached
branch_authorization: none
updated_at: 2026-10-06T03:54:28Z
scope:
  - src/agent/loop/question_form_tests.rs
---

# Peer Work Record

## Assignment

Repair native HTTP fixture behavior supporting T061/T067 AC-3 and AC-9
without changing runtime or discussion/admission assertions.

## Actions

Created an isolated worktree through an atomic peer claim from the verified
combined implementation. Inspected the native macOS failures: both discussed
execution cases fail at the fixture request read with WouldBlock before the
required unresolved clarification assertions. The nonblocking listener leaves
accepted-stream blocking mode unspecified across platforms.

[Apple accept documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/accept.2.html)
describes accepted sockets retaining listener properties. Explicitly normalize
accepted streams to blocking mode before the unchanged five-second read/write
timeouts. Keep strict POST route, content length, JSON and byte bounds, the
30-second accept deadline and every actual discussion assertion unchanged.

Added a portable real-socket regression that forces nonblocking acceptance,
observes WouldBlock before normalization, verifies the bounded reader stays
pending while input is delayed and then receives the exact valid JSON payload.

## Validation

Source review and native author gates pending. The shared nib-question-form
reservation remains the applied 0.3.0 minor release from 0.2.0; no second bump.
All Cargo gates will use Task with one build job and debug symbols disabled.

## Blockers and Dependencies

None.

## Memory Impact

Status: none
Rationale: This bounded test-fixture repair creates no new durable product
decision or runtime contract. Shared delivery reconciliation remains pending.

## Next Step

Obtain independent source review and run Task fmt, check, test:agent-context
and docs:check before publishing an exact clean handoff.

## Handoff

Pending.
