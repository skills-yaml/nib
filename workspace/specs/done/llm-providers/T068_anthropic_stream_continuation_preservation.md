# T068: Preserve Anthropic Streamed Tool Continuations

**Status:** Done
State: done
Primary Feature: llm-providers

## Problem and Authority

The user authorized continuing [T023](../../development/llm-providers/T023_live_llm_provider_model_integration_qualification.md) with offline investigation and correction of evidenced adapter defects. Independent source review found that Anthropic streaming discards thinking/signature/redacted-thinking blocks and reconstructs the private continuation from aggregated text and tool calls. The completion path preserves native blocks. Anthropic requires the complete unmodified thinking blocks accompanying tool use to return with tool results.

This compatible repair follows the shipped [T022](../../done/llm-providers/T022_provider_neutral_llm_contract_and_adapter_conformance.md) contract. T023's retained refusal used completion for both requests; the streaming defect is not established as that refusal's cause. T023 stays in development and its live acceptance remains unresolved.

## Scope and Proposed Behavior

Capture the ordered native streamed content privately, including initial fields, text block boundaries, thinking and signature fragments, opaque redacted-thinking data, and tool arguments. At a valid completed tool turn, bind this reconstructed native content to the existing one-use provider/model/session/run continuation. Tool outputs keep their existing invocation/call correlation and error classification.

Public projected events remain authorized text and tool chunks only. Thinking, signatures and encrypted data stay in ephemeral private state. Retain stream/item/continuation bounds and reject malformed or unfinished native content before granting tool authority. Keep deterministic fixture coverage for completion parity, single/parallel tool results, omitted thinking, refusal and terminal failures.

## Exclusions and Compatibility

No provider/model substitution, prompt or effort change, refusal suppression, alternate-prompt retry, credential read, paid call, governed instruction edit, or production rollout is included. Public request types, engine persistence and workload semantics remain unchanged. A live qualification pass requires separately authorized protected runs with budgets and retained evidence.

## Affected Areas

- `src/llm/anthropic.rs` and a cohesive private native-content helper if needed.
- `src/llm/conformance_tests.rs` and focused offline fixture modules.
- Native package versions, release reservation, spec catalog, linked T023 record, human work record and durable memory.

## Ordered Implementation Plan

1. Preserve user changes in the primary checkout; use one isolated author branch and read-only independent review.
2. Reserve patch 0.3.1 from verified published development 0.3.0 through the shared transaction; the designated author applies it once before implementation artifacts.
3. Add a valid signed-thinking/redacted-thinking streamed round-trip fixture and demonstrate its failure on the existing adapter.
4. Implement bounded private indexed native-content reconstruction; prove exact outgoing assistant blocks, privacy, correlation and failure paths.
5. Run focused Task gates, reconcile records/memory and obtain two-stage independent exact-candidate review.
6. Freeze the candidate; run native complete gates and exact-merge Linux/macOS/Windows CI. Deliver the identical verified revision to development and main, reconciling lifecycle only from observed events. Publication remains separate.

## Acceptance Criteria

- [x] AC-1: A valid streamed tool turn preserves original native block order, boundaries, thinking text, split signatures, redacted data and tool argument values in the outgoing continuation.
- [x] AC-2: Thinking with empty displayed text retains its signature; completion and streaming construct equivalent native continuations for equivalent responses.
- [x] AC-3: Single and parallel tool results preserve provider call IDs, neutral invocation correlation, classification and one-use scope guards.
- [x] AC-4: Private thought/signature/redacted values appear in no public stream delta or debug projection; existing byte/item/continuation bounds remain enforced.
- [x] AC-5: Malformed or unfinished native blocks, missing terminal, truncation and refusal cannot produce executable tool authority; normal text/tool streaming stays compatible.
- [x] AC-6: Independent exact-candidate review and fresh Task/native platform gates pass; versions, catalog, documentation, memory and confirmed development/main delivery are reconciled.

## Validation Gates

Focused iteration: `task test:llm-conformance`, `task test:llm-live:offline`, `task versions:check`, and affected documentation checks. Add failing reproduction before the production fix. All Cargo operations are serial with a cache affine to the author worktree.

Frozen native candidate: `task verify`, `task docs:check`, `task versions:check`. Exact merge CI must pass Linux/macOS/Windows, native all-target checks, optimized binary identity/clean-source qualification and native interaction smoke; Linux runtime coverage must meet the existing 80% threshold. Gate evidence must identify the actual integration/main revision. No live-provider invocation is a completion gate for this offline bug fix.

## Risks and Rollback

Opaque provider blocks must remain private and bounded. A malformed stream fails closed instead of inventing or dropping required blocks. Preserve unknown initial metadata without exposing it in diagnostics. Review compatibility of synthetic fixtures and native sequence handling. Reverting the isolated codec repair restores the prior behavior; a published package version is never decremented or reused.

## Source Evidence

- [Anthropic thinking preservation](https://platform.claude.com/docs/en/build-with-claude/thinking#preserving-thinking-blocks), inspected 2026-10-06.
- Existing `complete_anthropic_stream` synthesized continuation and `AnthropicStreamParser` ignored private deltas at base `94539d051a99df7e082ee98596799856b914c22f`.
- T023's retained 2026-09-25 canary used `context.complete` for its initial and tool-result requests; its hosted artifact listing is now empty. Preserve its historical failure classification.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-anthropic-stream | Compatible repair of native streamed tool-continuation preservation; atomic target 0.3.1 from published 0.3.0, development-start. |

## Memory Impact

Status: updated
Rationale: Appended the native streaming preservation contract and separate unresolved completion-path refusal to [workspace/agents/memory/facts.md](../../../agents/memory/facts.md) and [workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md); no private provider content is stored.

## Offline Acceptance Evidence

The pre-fix local HTTP regression failed at `46a5a51b0de263deffa5d316a0578db78b288d0f`: outgoing assistant content collapsed six native blocks into aggregated text and two tool calls; 26 existing conformance cases passed. After repair, `task test:llm-conformance` passes 31 cases, including exact native assistant arrays, completion/stream parity, single explicit-error and parallel success results, private projection checks, malformed/unfinished/after-terminal rejection, refusal and continuation byte/item limits. Existing conformance covers one-use scope and invocation guards. `task test:llm-live:offline` passes 71 tests with the paid qualification ignored.

These credential-free results support AC-1 through AC-5. AC-6 is supported by the exact reviews, frozen native/hosted gates and observed development/main delivery below. Completion records receive fresh review and verification before their own shared delivery. T023's historical complete-path refusal remains unexplained.

## Integration Evidence

Revision: 9762df5c10727889adff072045fcd8a8c1369dff
Outcome: passed

Shared development was verified at `9762df5c10727889adff072045fcd8a8c1369dff` on 2026-10-06T11:58:18.336608+00:00. Frozen native task verify/docs:check/versions:check and exact-merge Linux/macOS/Windows CI 37453323677 passed, including clean exact-source optimized binary qualification and native interactions. This observed integration establishes test state. Main delivery is confirmed below; publication remains separately tracked.

## Main Merge Evidence

Revision: 9762df5c10727889adff072045fcd8a8c1369dff
Outcome: passed

The SAME qualified revision advanced to shared main on 2026-10-06T12:01:05.731122+00:00. Its remote ref was verified immediately; GitHub confirms [PR49](https://github.com/skills-yaml/nib/pull/49) merged on 2026-10-06T12:01:06Z at this revision. Independent local Test-record review approved `9ab6d564d0816bfbd97068eeb0199767f7904ef1` before main delivery; that committed test state retains the normal lifecycle.

## Acceptance Reconciliation

| Criterion | Supporting evidence |
| --- | --- |
| AC-1 / AC-2 | Exact native six-block outgoing HTTP comparison and completion/stream parity, including split signatures, opaque metadata/redaction and empty displayed thinking. |
| AC-3 | Parallel reversed-order neutral outputs preserve exact provider IDs/payloads and success classification; single explicit error output preserves its ID/payload/classification; existing conformance covers one-use and scope guards. |
| AC-4 | Projected events/response/error Debug exclude private sentinels; oversized private bytes/items reject continuation. |
| AC-5 | Reordered/overlapping/restarted/unfinished blocks, missing terminal, truncation, refusal and after-terminal content reject authority; existing native text/tool fixtures pass. |
| AC-6 | Independent exact author/merge and Test-record reviews; frozen clean native verify/docs/versions and all-platform CI 37453323677; Linux 84.54% coverage; actual identical development/main events above; versions/catalog/memory reconciled. Final record delivery has its own fresh exact review and full gates. |

No live acceptance is inferred for T023. The compatible patch remains 0.3.1, applied once; production rollout and development publication are separate observed events.

## Development Publication Evidence

Development publication was independently verified on 2026-10-06: Release Artifacts 37459973513 completed successfully, including all four native archive builds and publication. The public development-latest nib-release.json binds version 0.3.1 to exact revision 9762df5c10727889adff072045fcd8a8c1369dff, published at 2026-10-06T12:10:14Z. Its manifest SHA-256 is f83f51f5635cda345bbd58e153d144e49d1127c5c25bfcc5c532c76f9e43ceea; all four archive sizes and SHA-256 digests match GitHub uploaded-asset metadata. Released means development-channel publication; production was separately inspected at 0.1.0, revision 15123a3ef275458efc87200400219aeacc3e9ea9. No second bump or production rollout is implied.

| Archive | Bytes | SHA-256 |
| --- | ---: | --- |
| nib-linux-x86_64.tar.gz | 13070461 | `549e752486ca025656624af275cba15f7fa70f7cddec6bb8bf706ecd00c807a2` |
| nib-macos-aarch64.tar.gz | 11205065 | `927e64f82b322e8b14f8c8d8447b64d826d9c5cb3e5e861d4c2b7f65923e1482` |
| nib-macos-x86_64.tar.gz | 11829733 | `b5d301c4628267c74a54e7bde3c657b496ae01dcb9a1727219d0b1b88a58b57d` |
| nib-windows-x86_64.zip | 11459390 | `879a742320b31a6c4272f84e5a6bc4b44ae3f0a06b7d2b2ff2d8775700c58158` |

The immutable source revision and manifest digest identify this publication even when development-latest is subsequently refreshed.
