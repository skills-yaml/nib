# T003: Implement Context Engine with Dynamic Compression and Session Management

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

## Problem Statement

Current nib context handling (in `context/` loaders) assembles AGENTS.md, skills, project standards, and workload snapshots statically. This leads to bloated contexts with raw tool outputs, terminal dumps, and unprocessed history. As sessions grow, this causes high token usage, degraded reasoning quality, and loss of long-term session state across restarts. Without dynamic compression and proper session modeling (as indexed alternating message sequences), the agent cannot efficiently retain crucial facts while reclaiming context space. This mirrors common failure modes in autonomous agents where context bloat reduces accuracy and increases costs.

## Goals

- Implement a Context Engine that dynamically compresses context once thresholds are reached (e.g., 50% of limit), synthesizing historic facts and progress while preserving system elements, AGENTS.md rules, active skills, and key workload state.
- Model sessions as durable, indexed sequences of alternating user/assistant/tool messages with strict role alternation invariants.
- Support cross-session persistence of session histories and discrete factual memory (environment configs vs. user identity).
- Integrate seamlessly with existing context loaders, ToolExecutor, WorkloadStore, and permissions model.
- Enable the runtime loop to maintain efficiency over long-running tasks and multi-turn interactions.
- Provide hooks for the ASCII sequence diagram in T002 to reflect compression triggers and session appends.

## Non-Goals

- Full replacement of the current static context assembly (enhance and extend it).
- Inventing new LLM architectures for compression (use standard auxiliary LLM calls or simple heuristics initially).
- Handling non-text modalities (focus on text-based sessions and tool outputs).
- Real-time streaming compression (batch at turn boundaries).

## Proposed Design

Extend the existing `src/nib/context/` module and integrate with `core/` runtime components.

**Core Components:**
- **Context Builder**: Enhances current `assemble_context()` to track token usage against model `context_length` (from config).
- **Compression Trigger**: When threshold hit (configurable, default 0.50):
  - Preserve: System prompt, AGENTS.md content, active skills instructions, current task state.
  - Compress: Historic conversation logs and raw tool results by sending to an auxiliary LLM with instructions to "summarize historic facts, code progress, decisions, and lessons learned into a compact narrative".
  - Replace: Old logs with the synthesized summary + metadata (original turn range, key entities).
  - Target: Reduce to ~20% of original size for the compressed segment.
- **Session Manager**: Store sessions as indexed lists (e.g., in SQLite or JSONL alongside WorkloadStore). Enforce:
  - Strict alternation: User → Assistant (tool call or text) → Tool (if any) → Assistant (resolution).
  - No consecutive same-role messages (squash or error on violation).
- **Memory Store**: Add a discrete layer (separate from main workload SQLite):
  - Environment memory: Key-value for facts, preferences, learned behaviors (JSON-backed or SQLite table).
  - User memory: Identity records, long-term profile.
  - Persist across sessions/profiles.
- **Integration Points**:
  - Call from runtime state machine (see T005).
  - Feed compressed context into ToolExecutor for tool decisions.
  - Update WorkloadStore with session snapshots and compression events for audit.
  - Respect permissions: Compression decisions logged as tool-like events if they affect execution.
- **Config** (align with T007): Add to schema:
  ```yaml
  compression:
    enabled: true
    threshold: 0.50
    target_ratio: 0.20
  memory:
    enabled: true
    provider: "built-in"  # or "sqlite"
  ```

**Implementation Approach:**
- New `src/nib/context/compression.py` for the trigger and LLM summarizer.
- Enhance `src/nib/context/loader.py` to produce compressed views.
- Add session models in `core/models.py` (e.g., `Session` with indexed messages).
- Use existing aiosqlite for persistence; add tables for sessions and memory KV.
- Expose via new methods in Context Loader: `build_compressed_context()`, `append_to_session()`.
- For the sequence diagram (in T002): Insert steps for "13. [Optional] Context Compression Trigger" and "12. Append to chat context".

This design reuses nib's existing workload and permission infrastructure while adding the missing compression and session durability from the target architecture.

## Alternatives Considered

- Simple truncation or sliding window: Rejected — loses too much factual history and violates "retaining crucial session context".
- Always use full history with external RAG: Rejected for v1 — adds complexity; prefer in-context compression first for low-latency local agent.
- External vector DB for memory: Rejected initially (use simple KV for now; can evolve in rollout).
- No strict role alternation enforcement: Rejected — core invariant from the spec to prevent malformed prompts.

## Risks and Tradeoffs

- **Compression Fidelity Risk**: Summaries may drop subtle details (mitigation: preserve key entities/links in metadata; make summarizer prompt configurable; test on real coding sessions).
- **Performance Tradeoff**: Auxiliary LLM call for compression adds latency and cost (tradeoff acceptable for long sessions; optional and threshold-based).
- **Storage Growth**: Session histories + memory KV can bloat (mitigation: tie cleanup to maintenance daemons in T004; retention policies).
- **Integration Complexity**: Must not break existing ToolExecutor or permissions flows (design keeps compression as a pre-execution step in the loader).

## Rollout Plan

1. **Phase 1 (Foundation)**: Implement basic compression trigger and session append in context module. Add models and persistence. Wire into current demo-tool for testing.
2. **Phase 2 (Integration)**: Connect to runtime state machine (T005). Update ToolExecutor to use compressed context. Add logging of compression events to workload.
3. **Phase 3 (Persistence & Memory)**: Separate Memory Store. Support cross-session load/save.
4. **Phase 4 (Polish)**: Config integration (T007), tests (T008), and diagram validation. Update architecture.md and FT-002.
5. Use subagent-driven-development (from existing skills) for implementation steps. Validate against the sequence diagram in T002.

## Validation and Acceptance Criteria

- Context compresses when threshold reached, reducing size while retaining system/AGENTS/skills facts (measured via token count before/after).
- Sessions stored as indexed alternating sequences; role invariant enforced (no consecutive same roles).
- Memory Store persists discrete env/user data across restarts and sessions.
- Compression and append steps appear correctly in the end-to-end sequence diagram execution.
- All paths respect permissions (e.g., compression decisions audited).
- `task test` and `task check` pass; demo-tool shows compressed context in long interactions.
- Matches symphony structure: clear problem, goals, design, risks, etc.

## Open Questions

- Exact auxiliary LLM prompt for summarization (tunable via skills?).
- How to handle very large tool outputs (e.g., pre-compress before session append)?
- Integration with existing workload "backlog/working/done" buckets (should compression affect task status?).
- Performance benchmarks for compression in real coding workloads.