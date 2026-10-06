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
- Independent preparation and exact production reviews passed; inventory and sequence findings were resolved. Frozen native/all-platform gates passed at the confirmed integration revision below. Main delivery is confirmed below.

## Independent Review Reconciliation

The initial production review found that indexed native content could be silently sorted despite reversed or overlapping wire starts. The accepted fix flags a start whose index is not the next sequential index or whose predecessor remains open, and rejects tool authority at completion. Malformed-order and overlap HTTP regressions now accompany the earlier malformed fixtures. Existing text-only projection remains compatible.

Delivery follows the normal lifecycle: qualify the exact PR merge revision Q; fast-forward shared development to Q and confirm it; commit the corresponding local test-state record T with catalog/release paths and fresh documentation/version gates; then deliver the same fully qualified Q to main. A later done-state record D includes both observed events and receives fresh exact review and qualification before shared delivery. Main never receives an unqualified metadata revision.

## Confirmed Development Event

Shared development is verified at `9762df5c10727889adff072045fcd8a8c1369dff` on 2026-10-06T11:58:18.336608+00:00. Frozen native verify/docs/versions and exact all-platform CI 37453323677 passed. T068 moves to test with matching catalog and reservation paths; main/publication remain pending. This local committed Test record precedes delivery of the same qualified Q to main.

## Confirmed Main Event and Completion

The same exact qualified `9762df5c10727889adff072045fcd8a8c1369dff` reached shared main on 2026-10-06T12:01:05.731122+00:00; remote verification and PR49's 2026-10-06T12:01:06Z merge receipt agree. The independent review approved committed local Test `9ab6d564d0816bfbd97068eeb0199767f7904ef1` before that event. T068 is reconciled to done with matching catalog/reservation paths and memory; the current catalog has 78 done, T023 development, and two backlog specs.

All six T068 criteria have exact fixture/review/gate/event evidence. T023 remains unresolved because the retained refusal used completion; the source-level audit and offline tests do not establish its cause. No paid call or model/prompt change occurred. Final records receive fresh exact review, native verify/docs/versions and all-platform merge-checkout qualification before shared delivery. Development publication is separately inspected; production remains protected.

## Development Publication Evidence

Development publication was independently verified on 2026-10-06: Release Artifacts 37459973513 completed successfully, including all four native archive builds and publication. The public development-latest nib-release.json binds version 0.3.1 to exact revision 9762df5c10727889adff072045fcd8a8c1369dff, published at 2026-10-06T12:10:14Z. Its manifest SHA-256 is f83f51f5635cda345bbd58e153d144e49d1127c5c25bfcc5c532c76f9e43ceea; all four archive sizes and SHA-256 digests match GitHub uploaded-asset metadata. Released means development-channel publication; production was separately inspected at 0.1.0, revision 15123a3ef275458efc87200400219aeacc3e9ea9. No second bump or production rollout is implied.

| Archive | Bytes | SHA-256 |
| --- | ---: | --- |
| nib-linux-x86_64.tar.gz | 13070461 | `549e752486ca025656624af275cba15f7fa70f7cddec6bb8bf706ecd00c807a2` |
| nib-macos-aarch64.tar.gz | 11205065 | `927e64f82b322e8b14f8c8d8447b64d826d9c5cb3e5e861d4c2b7f65923e1482` |
| nib-macos-x86_64.tar.gz | 11829733 | `b5d301c4628267c74a54e7bde3c657b496ae01dcb9a1727219d0b1b88a58b57d` |
| nib-windows-x86_64.zip | 11459390 | `879a742320b31a6c4272f84e5a6bc4b44ae3f0a06b7d2b2ff2d8775700c58158` |

The immutable source revision and manifest digest identify this publication even when development-latest is subsequently refreshed.
