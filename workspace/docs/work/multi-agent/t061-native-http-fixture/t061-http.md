---
schema_version: 1
coordination_id: t061-native-http-fixture
agent_id: t061-http
role: implementer
status: active
base_revision: d8b8d82fa55fb7f5f5f32ae1da87a1ccf157ea88
task_ref: detached
branch_authorization: none
updated_at: 2026-10-06T04:06:03Z
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
observes WouldBlock before normalization, synchronizes reader readiness,
verifies the bounded reader stays pending while input is delayed and then
receives the exact valid JSON payload.

Independent source review confirmed normalization and retained boundaries.
Its requested reader-ready synchronization is incorporated before frozen
qualification. The [Linux accept manual](https://man7.org/linux/man-pages/man2/accept.2.html)
explicitly distinguishes Linux noninheritance from canonical BSD behavior and
requires portable callers to set the needed accepted-socket flags.

## Validation

Task fmt and renewed Task fmt passed. Strict task check passed, including
all-target/all-feature warning-denying Clippy and native workspace, version and
coordination validators. Initial and stable-source renewed task
test:agent-context each passed all 201 cases: 91 context, 108 agent and two
build-metadata tests. Both discussed execution cases that failed on native
macOS and the portable forced-nonblocking delayed-payload regression pass.
Task docs:check passed all native validators and five integrity tests;
documentation validation is renewed after these final records.

Self-review ties the repair to AC-3's real discussion/dependency behavior and
AC-9's native deterministic qualification. Runtime, POST route, content-length
and JSON rejection, request byte bounds, five-second I/O timeouts, 30-second
accept deadline and actual discussion assertions remain unchanged.
Independent source review confirmed the repair and requested reader readiness;
that finding is resolved before the renewed source gate. Exact final review,
registered full integration and hosted native qualification remain required.
The shared nib-question-form reservation remains the applied 0.3.0 minor release
from 0.2.0; no second bump. All Cargo used Task with one build job and debug
symbols disabled, with strict native bwrap tests required.

## Blockers and Dependencies

None.

## Memory Impact

Status: none
Rationale: This bounded test-fixture repair creates no new durable product
decision or runtime contract. Shared delivery reconciliation remains pending.

## Next Step

Publish the exact clean handoff for renewed independent review, registered
full native integration and hosted native qualification.

## Handoff

Author formatting, strict static, focused behavior (201) and documentation (5)
gates passed. Exact final independent review, full transactional integration
and hosted qualification remain required; no shared branch delivery or lifecycle
transition is claimed.
