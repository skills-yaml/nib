# T004: Profiles, Discrete Memory Store, and Maintenance Daemons (Cron/Curator)

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

## Problem Statement

nib currently uses a monolithic WorkloadStore (SQLite for projects/tasks/tool history) without separation of concerns for different runtime workspaces. This leads to:
- Fragile state: No per-workspace "Profiles" that encapsulate custom env, skills, and context DBs, making it hard to switch contexts or isolate user data.
- Loss of cross-session memory: Behaviors, preferences, and learned facts are not persisted in discrete stores, causing repetitive steering.
- No automated maintenance: Old sessions, memory bloat, and stale data accumulate without Cron-like recurring jobs or Curator-style cleanup.

This directly conflicts with the need for durable, isolated, self-improving agent runtimes that persist lessons across sessions without manual intervention.

## Goals

- Introduce Profile concept: Per-workspace runtime isolation with custom .env, skills, and localized stores.
- Discrete Memory Store: Separate environment configurations (memory) from user identity records (user) in a durable (SQLite/JSON) format.
- Maintenance Daemons:
  - Cron: For offline recurring jobs (e.g., scheduled task reviews, background compression).
  - Curator: For cleaning old memory, sessions, and skills (with pinning for important items).
- Cross-session persistence of facts, preferences, and progress.
- Integration with ToolExecutor, permissions, context engine (T003), and workload buckets (backlog/working/done).
- Support for the runtime state machine (T005) and config (T007).

## Non-Goals

- Full distributed or cloud-based profiles (keep local-first).
- Complex multi-user tenancy (focus on single-user per profile).
- Real-time daemon execution in the main loop (offline/background where possible).

## Proposed Design

Extend `core/` and add new modules for profiles/memory/daemons.

**Core Additions:**
- **Profile**: Dataclass/model identifying a workspace (id, root_path, custom_env, active_skills, memory_db_path, context_db_path). Loaded at startup or task activation. Maps to current project model but adds isolation.
- **Memory Store**: Discrete key-value (env memory for facts/behaviors; user memory for identity/preferences). Backed by SQLite (new tables or separate DB) or JSON for portability. Persist facts like "preferred test command", learned patterns, user style.
- **Maintenance Daemons**:
  - Cron: Scheduler (extend existing task runner or use Python `schedule`/`apscheduler`) for recurring tasks, e.g., "every 24h: compress old sessions", "on idle: review backlog".
  - Curator: Background process to archive/delete old data (sessions > N days, uncompressed history), with "pinned" exceptions for important skills/memory. Telemetry for usage.
- **Integration**:
  - Context Engine (T003) loads profile-specific memory and sessions.
  - ToolExecutor records against current profile.
  - WorkloadStore extended with profile_id foreign keys.
  - Daemons update workload (e.g., move stale tasks) and respect permissions (no destructive cleanup without policy).
- **Persistence**: Add to WorkloadStore or new `memory.py`:
  - Tables: profiles, memory_env, memory_user, sessions (indexed messages), daemon_logs.
  - Cross-session: Load profile on `nib init` or task start.

**Config Extension** (align with T007):
```yaml
profiles:
  default: "my-project"
  active:
    - id: "my-project"
      root: "/path/to/project"
      env: ".env.nib"
memory:
  enabled: true
  provider: "sqlite"
daemons:
  cron_enabled: true
  curator_enabled: true
  retention_days: 30
```

## Alternatives Considered

- Single monolithic store (current state): Rejected — leads to the fragile state problem.
- Full ORM like SQLAlchemy: Rejected for v1 (keep aiosqlite simple; evolve later).
- External services (e.g., vector DB for memory): Rejected (local-first; use built-in for now).
- In-memory only daemons: Rejected (need durability for cross-session).

## Risks and Tradeoffs

- **Complexity Risk**: Adding profiles/memory/daemons increases surface (mitigation: modular, start with defaults, extensive tests in T008).
- **Performance Tradeoff**: Daemons add background overhead (tradeoff for long-term usability; make configurable and low-priority).
- **Migration**: Existing users' data needs migration to profiles (plan: one-time script in rollout).
- **Isolation vs. Sharing**: Strong profiles may hinder cross-project learning (mitigation: optional "global" profile or shared memory subsets).

## Rollout Plan

1. **Phase 1**: Define Profile and Memory Store models/persistence. Basic load/save.
2. **Phase 2**: Implement Cron (recurring compression/review) and Curator (cleanup with pinning).
3. **Phase 3**: Integrate with T003 (context uses profile memory/sessions), T005 (runtime uses profile), ToolExecutor (records per profile).
4. **Phase 4**: Config support (T007), tests (T008), update architecture.md and docs. Provide migration for T001-era data.
5. Use existing skills (e.g., symphony for planning daemon jobs).

## Validation and Acceptance Criteria

- Profiles load custom env/skills per workspace; isolation verified (no cross-profile leakage).
- Memory Store persists facts across sessions/restarts (env vs. user separation).
- Daemons run (Cron jobs execute; Curator cleans old data, respects pins).
- Integration with workload buckets and permissions (daemons log as tool executions).
- Matches symphony structure and sequence diagram (T002) for persistence steps.
- `task test` passes; "nib doctor" (T007) validates daemons.

## Open Questions

- Retention policies and pinning UI (TUI/CLI commands?).
- How daemons interact with active TUI sessions (pause on user activity?).
- Exact schema for memory KV (simple strings or structured with timestamps?).
- Performance impact of frequent curator runs on large histories.