# T068: Preserve Anthropic Streamed Tool Continuations

**Status:** Test
State: test
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
- [ ] AC-6: Independent exact-candidate review and fresh Task/native platform gates pass; versions, catalog, documentation, memory and confirmed development/main delivery are reconciled.

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

These credential-free results support AC-1 through AC-5. AC-6 remains open until exact independent review, frozen native/hosted qualification and observed shared development/main delivery. T023's historical complete-path refusal remains unexplained.

## Confirmed Shared Integration

Shared development was verified at `9762df5c10727889adff072045fcd8a8c1369dff` on 2026-10-06T11:58:18.336608+00:00. Frozen native task verify/docs:check/versions:check and exact-merge Linux/macOS/Windows CI 37453323677 passed, including clean exact-source optimized binary qualification and native interactions. This observed integration establishes test state. Main delivery and publication remain separate pending events.
