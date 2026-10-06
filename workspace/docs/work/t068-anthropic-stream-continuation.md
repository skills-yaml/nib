# T068 Work Plan

One isolated author branch repairs native Anthropic streamed continuation preservation under linked T068. Independent review is read-only. Primary skill-pin edits remain outside this change.

## Scope

Declared areas: Anthropic adapter/private native-content helper, offline conformance fixtures, package versions/release ledger, T068/T023/catalog, this work record and durable memory. No governed instruction or live-provider changes.

## Reservation

Atomic `nib-anthropic-stream` patch target 0.3.1 from public development 0.3.0, checked at main/development base `94539d051a99df7e082ee98596799856b914c22f`. Designated author `t068-author` applies Cargo and skill package version once before implementation. Recheck shared occupancy at handoff.

## Steps and Gates

1. Demonstrate the native streamed round-trip defect with a deterministic local HTTP fixture.
2. Preserve indexed native blocks in private state and retain public projection, scope/correlation, bounds and refusal guards.
3. Run focused conformance/live-harness offline Task gates; reconcile known memory and acceptance.
4. Obtain independent scope then quality review; freeze the candidate and run complete native/hosted gates.
5. Deliver the exact qualified revision and reconcile lifecycle from actual events; publication separate.

## Memory Impact

Status: updated
Rationale: Recorded the verified native-stream preservation fact and its distinction from T023's unresolved complete-path refusal in [workspace/agents/memory/facts.md](../../agents/memory/facts.md) and [workspace/agents/memory/changelog.md](../../agents/memory/changelog.md).

## Offline Results

- Red regression at `46a5a51b0de263deffa5d316a0578db78b288d0f`: exact native continuation failed with all 26 earlier conformance cases passing.
- Private indexed reconstruction now retains opaque thinking/signature/redacted fields, initial metadata and native block order. Validated tool values and existing neutral correlation remain authoritative. Fixed diagnostics contain no raw native values.
- Focused conformance: 31 passed. Offline live harness: 71 passed, 1 paid test ignored. No credentialed or paid requests.
- Existing signed-stream unit fixture now includes its valid native block starts/stops; the intentionally incomplete EOF rejection fixture remains negative.
- Independent preparation review resolved the inventory-count finding. Production candidate review, frozen gates and delivery remain pending.

## Independent Review Reconciliation

The initial production review found that indexed native content could be silently sorted despite reversed or overlapping wire starts. The accepted fix flags a start whose index is not the next sequential index or whose predecessor remains open, and rejects tool authority at completion. Malformed-order and overlap HTTP regressions now accompany the earlier malformed fixtures. Existing text-only projection remains compatible.

Delivery follows the normal lifecycle: qualify the exact PR merge revision Q; fast-forward shared development to Q and confirm it; commit the corresponding local test-state record T with catalog/release paths and fresh documentation/version gates; then deliver the same fully qualified Q to main. A later done-state record D includes both observed events and receives fresh exact review and qualification before shared delivery. Main never receives an unqualified metadata revision.

## Confirmed Development Event

Shared development is verified at `9762df5c10727889adff072045fcd8a8c1369dff` on 2026-10-06T11:58:18.336608+00:00. Frozen native verify/docs/versions and exact all-platform CI 37453323677 passed. T068 moves to test with matching catalog and reservation paths; main/publication remain pending. This local committed Test record precedes delivery of the same qualified Q to main.
