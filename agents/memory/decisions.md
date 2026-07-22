# Decisions

## 2026-06-17 - Adopt workspace-docs@1.0.0

- Type: decision
- Source: user
- Confidence: high
- Review: none
- Supersedes: none

Content:

nib adopts the workspace documentation standard at `workspace-docs@1.0.0`. Adoption is additive: preserve project-specific guidance and legacy specs, and keep generated agent context inside `AGENT-CONTEXT` markers.

## 2026-06-19 - LLM Integration and Agent Loop (FT-004)

- Type: decision
- Source: user + planning
- Confidence: high
- Review: none
- Supersedes: previous global workload model (replaced by per-project .nib/sessions/)

Content:

Adopt FT-004 for LLM-driven agent loop. Key decisions:
- Sessions (conversations + tool calls) are the primary memory, persisted as JSON files in <project>/.nib/sessions/.
- No central global projects/tasks in the runtime persistence layer.
- Pluggable LLMClient (Grok-first).
- AgentLoop routes every action through ToolExecutor (hybrid bwrap + worktrees + boundaries + plan gates).
- Rich context from AGENTS.md + skills + session history.
- Plan Mode support before execution.
- Full audit trail in session files.
- Leverage existing ecosystem (MCP, subagents, skills) instead of duplicating.

This completes the shift from workload-centric to session + execution-centric architecture while preserving safety and human steerability.

## 2026-06-20 - Rust CLI Rewrite + LLM/Agent Loop Merged (PR #1)

- Type: milestone / decision
- Source: user + implementation
- Confidence: high
- Supersedes: Python-only CLI (Typer)

Content:

Merged feat/implement-basic-agent-tools (PR #1) into main.

- Primary CLI is now the Rust binary (`nib` via clap).
- `nib chat` supports only `/model` for switching (list + number select or direct name for active provider).
- `nib auth` wizard for multi-provider configuration.
- Hybrid: Rust CLI + Python core (LLM via LiteLLM, agent loop, tools, sessions in `.nib/`).
- FT-003 (hybrid bwrap sandbox) and FT-004 (LLM + agent loop) completed and moved to done/.
- CI, release, and install scripts follow skm patterns.
- All execution still updates the authoritative session state in `.nib/sessions/`.

Branch deleted locally. Main is now at merge commit e47cb7f.

Note: FT-003 was later **reopened** (2026-07-02) — sandbox was never implemented despite this milestone text.

## 2026-07-02 - FT-005 Pure Rust Migration — scope locked

- Type: decision
- Source: user
- Confidence: high
- Review: none
- Supersedes: hybrid Python core as long-term architecture (transitional until FT-005 Phase 6)

Content:

Approved FT-005 move to `development/` with these scope decisions:

- **Config:** Migrate to `.nib/config.toml`; auto-migrate from legacy `config.json`.
- **LLM:** Full provider set day one in Rust — OpenAI, Anthropic, Google Gemini, Grok (xAI), OpenRouter, Mock (no LiteLLM, no phased provider rollout).
- **TUI:** Port to ratatui in FT-005 Phase 4 (`nib tui`); in scope, not deferred.
- **FT-003:** Reopen to `development/`; implement hybrid sandbox only in Rust (FT-005 Phase 5 / T019).

Next implementation unit: T009 (module layout + TOML config migration).

## 2026-07-15 - Upgrade workspace-docs adoption to 1.2.0

- Type: decision
- Source: repo standard + implementation audit
- Confidence: high
- Review: none
- Supersedes: 2026-06-17 - Adopt workspace-docs@1.0.0

Content:

nib uses `workspace-docs@1.2.0`. Specs use the canonical `backlog/`, `development/`,
and `done/` lifecycle directories; legacy `feature/` and `task/` directories are
reference-only. Internal documentation links and done-state invariants are validated
by `task docs:check`.

## 2026-07-15 - Profile-scoped runtime state and hybrid execution defaults

- Type: decision
- Source: implementation audit
- Confidence: high
- Review: none
- Supersedes: 2026-06-19 session path and 2026-06-20 transitional runtime claims

Content:

Runtime state is isolated under `.nib/profiles/<id>/` by default: sessions, memory,
context, managed skills, and daemon state use the selected profile. The project-level
`.nib/config.toml`, session/subagent worktrees, and delegation records remain shared
coordination state. Mutating tools use Git worktrees. The default shell provider is
`hybrid`: use `bwrap` when usable, otherwise execute directly in the worktree; the
explicit `bwrap` provider fails closed. Every agent-selected tool call is routed
through `ToolExecutor` and recorded in the profile session.

## 2026-07-15 - Runtime persistence and transport boundaries

- Type: decision
- Source: implementation audit
- Confidence: high
- Review: none
- Supersedes: historical T002 SQLite/global backlog and T006 HTTP/OAuth v1 proposals

Content:

The authoritative runtime workload is profile-scoped session JSON containing
structured `PlanStep` state, lifecycle events, and audited tool calls, supplemented by
profile-scoped durable daemon task records. nib does not ship a global SQLite backlog.
The shipped v1 MCP client and server use stdio; HTTP/SSE transports and OAuth remain
future work. Telegram, Slack, and Discord authentication, listeners, and reply
delivery remain outside nib in provider adapters, which call nib through its
normalized, tool-schema-closed gateway.

## 2026-07-15 - Distinguish namespace quarantine from exact Unix deletion

- Type: decision
- Source: final persistence security review
- Confidence: high
- Review: none
- Supersedes: none

Content:

An identity-checked, no-replace move into quarantine proves which entry was detached
from the authoritative namespace. It does not prove that a subsequent pathname-based
Unix `unlink` physically removed that same inode when a hostile same-UID process can
replace the quarantine pathname after the final identity check. Specifications and
runtime evidence must call the implemented guarantee conditional namespace quarantine,
must not describe it as exact physical unlink, and must surface unverified residual
cleanup rather than claiming deletion.

## 2026-07-16 - Preserve managed-worktree artifacts without exact ownership proof

- Type: decision
- Source: final ownership review
- Confidence: high
- Review: none
- Supersedes: none

Content:

Managed-worktree compensation deletes a path, registration, or branch only when nib
has an exact ownership receipt that still matches the observed identity or object ID.
Failed-add and recovery paths preserve and report ambiguous or unproven artifacts
instead of guessing destructively. Current ownership receipts are process-local and
must not be described as durable cross-process proof.

## 2026-07-16 - Same-UID peer processes are outside the isolation boundary

- Type: decision
- Source: implementation and platform security review
- Confidence: high
- Review: none
- Supersedes: 2026-07-15 - Distinguish namespace quarantine from exact Unix deletion

Content:

nib treats repository data, stale writers, symlinks/reparse points, and every child it
starts as untrusted, but it does not claim isolation from a malicious peer process already
running as the same operating-system user. An unprivileged Unix process cannot unlink by
retained inode identity after a hostile pathname replacement, and Git cannot consume an
immutable repository-local configuration snapshot cross-platform without a stronger OS
broker. nib must continue to prove exact namespace detachment, validate configuration
before launch, retain ambiguous artifacts, and fail closed on observable races. Operators
requiring a hostile same-UID threat model must add an account, VM/container, or privileged
broker boundary.

## 2026-07-16 - Persist managed-worktree generational ownership

- Type: decision
- Source: FT-015 durable ownership implementation
- Confidence: high
- Review: none
- Supersedes: 2026-07-16 - Preserve managed-worktree artifacts without exact ownership proof

Content:

Managed subagent and session worktrees persist a versioned CAS ownership generation
under stable project `.nib` state before Git worktree creation. Receipt-ID-bound random
staging names carry pre-CAS provenance within the documented non-malicious-same-UID
boundary; staged identities are persisted before final publication, and the staged ref
remains as a hard-link generation anchor. The record retains the creation intent, path
and registration attribution, branch lineage and ref identity, object ID, serializable
filesystem identities, prior-anchor retirement, and per-artifact cleanup phases.
Restart recovery reopens those identities, preserves replacements and quarantine-only
state, resumes write-ahead cleanup, and retains a complete tombstone until deterministic
bounded compaction. Branch adoption rotates object ID, ref identity, and anchor through a
recoverable CAS transition. The namespace retains at most 64 records of 4 MiB each;
compaction uses a persistent-anchor kernel lock and collected tombstones fall back to a
deadline-bound non-destructive absence proof. Unattributed failed-add registrations
remain preserved.

## 2026-07-16 - Use one exclusive writer for rolling GitHub Releases

- Type: decision
- Source: release transaction review
- Confidence: high
- Review: none
- Supersedes: none

Content:

The repository release workflow is the exclusive writer for each rolling production
or development Release, its staging/backup records, and their tags. Publication is
serialized through the channel-specific GitHub environment; personal tokens, apps,
and other workflows must not retain equivalent mutation authority. GitHub Release
updates and deletes have no conditional compare-and-swap, so an immutable release plus
Git-CAS channel manifest is the required redesign if multiple writers become necessary.

## 2026-07-16 - Bind execution to one exact structured plan

- Type: decision
- Source: done-spec remediation and final quality review
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: implicit plan binding by mutable session state

Content:

An active run owns its session for the full loop through an OS-backed lease and retains
one immutable expected plan ID. Plan reuse requires an exact normalized-goal match and
a valid incomplete step structure. Approval and execution mutations use compare-and-
set semantics against that plan identity; replacement, malformed, stale-cursor, or
completed plans fail closed and are reconciled rather than inheriting stale authority.

Audit evidence is mandatory even for otherwise sessionless executor calls, but the
profile-scoped implicit session is evidence-only. Operational authority for plans,
schedules, and background work must always come from an explicit trusted session.
