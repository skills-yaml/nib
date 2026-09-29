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

## 2026-08-19 - Make self-update channel switching explicit and binary-owned

- Type: decision
- Source: user + T029
- Confidence: high
- Review: none
- Supersedes: FT-018 channel switching non-goal

Content:

Official managed builds may explicitly switch between the production and development
rolling channels with `nib update --channel prod|development`. The selected channel is
not stored in project or global configuration; the fully verified replacement binary's
embedded identity controls subsequent option-free updates and startup notices. A
different requested channel must replace the binary even when both manifests name the
same commit. Local/source, unsupported, non-writable, and ambiguous installations
remain installer-managed, and repository/tag selection remains compile-time controlled.

## 2026-08-19 - Treat plain and TUI as presentations of one interactive product

- Type: decision
- Source: user + T028 + T030
- Confidence: high
- Review: none
- Supersedes: 2026-06-20 chat-only `/model` capability and the separate chat/TUI product model

Content:

The line-oriented and full-screen interfaces must expose the same interactive agent,
session, command, model/provider, skill, MCP, approval, question, cancellation, and
reconciliation capabilities. Presentation and controls may remain native to each mode.
`nib` is the canonical interactive launcher, automatically selecting the TUI on a
capable terminal and plain mode otherwise; `--plain` and `--tui` force a presentation.
`nib chat` uses the same launcher, `nib tui` remains a compatibility alias, and
`nib run` retains its separate one-shot automation contract.

## 2026-08-20 - FT-019 TUI is a ledgered renderer over a shared interaction model

- Type: decision
- Source: user + FT-019 backlog revision
- Confidence: high
- Review: none
- Supersedes: none

Content:

FT-019 remains the umbrella interaction contract. The TUI is a presentation of that
contract, not a second product: typed activity transcript, wrapped composer, and
approval/question docks that keep the transcript visible. Conversation stays primary;
a permanent plan spine is rejected. `/` is canonical command discovery. Enter never
steers; the first user-visible slice is queue-only until the agent loop can bind
exact-run steering. Implementation still requires child development specs before code.

T039 later compacted chrome to one header row (folder + colored branch left, model +
context right) and a footer of approval mode plus agent mode. Speech renders markdown.

## 2026-09-02 - Keep v1 production delegation Linux-only

- Type: decision
- Source: FT-015/FT-017 closure review
- Confidence: high
- Review: pending exact-revision independent review
- Supersedes: none

Content:

FT-015 and FT-017 may close after native Windows/macOS mechanism tests and explicit
fail-closed production rejection pass; they do not wait for production enablement on
those platforms. The v1 production delegation boundary is Linux plus a usable bwrap PID
namespace. A protected cleanup authority outside the managed worker trust boundary is a
separate product capability owned by backlog spec FT-020. Windows and macOS may graduate
independently only through that future spec and must not weaken the current rejection
contract.

## 2026-09-25 - FT-020 ships fail-closed non-Linux production

- Type: decision
- Source: FT-020 selected design and C01-C07
- Confidence: high
- Review: 2026-09-25 spec-compliance then quality review
- Supersedes: 2026-09-02 backlog ownership of FT-020 as unimplemented future work

Content:

FT-020 is Done as the protected-owner implementation plus a fail-closed Windows
and macOS `production()` contract. Mechanism tests and Job/process-group
`current()` backends do not enable production. A later independent native
qualification record is required before `production()` may return a Windows or
macOS backend.

## 2026-09-02 - Qualify every native CI release binary exactly

- Type: decision
- Source: development-spec closure review
- Confidence: high
- Review: pending exact-revision hosted CI
- Supersedes: build-only native CI steps

Content:

Linux Validate, Windows Tests, and macOS Tests run `task qualify:llm-release` before
their platform smoke. The task embeds and verifies the checkout's exact commit, exercises
credential-free localhost LLM fixtures, hashes the optimized executable, and marks
evidence acceptance-eligible only when the source worktree is clean. A plain successful
release build is no longer sufficient T021/T022 closure evidence.

## 2026-09-02 - Close ordinary development specs only on exact native evidence

- Type: decision
- Source: development-spec closure review and exact hosted CI
- Confidence: high
- Review: independent spec-compliance and code-quality/security reviews
- Supersedes: pending review state for the 2026-09-02 native qualification decisions

Content:

T003, T004, T006, T007, T020, T021, T022, T026, T029, T034, T035, FT-015,
FT-016, FT-017, and FT-019 may move from `development/` to `done/` on exact run
`33683995100`. T021 and T022 close from the same final release-binary revision, T034
closes before FT-019 on the same native smoke evidence, and T029 composes that native
failure-boundary matrix with the already recorded real managed switches in both
directions. No live-provider authority is inferred: T023 remains in development.

## 2026-09-09 - Position nib simply as an AI agent

- Type: decision
- Source: owner product-positioning direction
- Confidence: high
- Review: canonical local verification and documentation integrity
- Supersedes: nib-specific "coding agent" and "coding/workload agent" product labels

Content:

The canonical product category for nib is **AI agent**. Public metadata, CLI help,
runtime identity prompts, architecture guidance, and product documentation use that
term without qualifying nib as a coding agent or workload agent. Coding, planning,
execution, and workload reconciliation remain concrete capabilities, not the product
category.

## 2026-09-28 - Make plain-language help a conversational request

- Type: decision
- Source: user + T055
- Confidence: high
- Review: `task verify` and documentation integrity
- Supersedes: T050's local command-registry response for plain-language help

Content:

Text such as `help` and `what can you do?` follows the ordinary interactive agent
route. The answer-only prompt receives bounded README, Taskfile, and supported
command metadata so it can give a repository-aware answer without creating a plan
when context is sufficient. `/help` remains the immediate, provider-free command
reference. Text entered during an active run follows the normal next-turn queue
rule.

## 2026-09-28 - Preserve changed worktrees and admit fresh work after Git identity changes

- Type: decision
- Source: T056
- Confidence: high
- Review: `task verify` and offline Mock worktree preparation

Content:

An unrelated completed worktree ownership receipt with an older common Git
directory identity may remain on disk while a fresh worktree is admitted below
the ownership store limits. Removing that receipt still requires exact Git
identity and ref recovery. When cached session ownership no longer validates,
preserve the worktree, registration, branch, and uncommitted files for inspection
instead of attempting automatic cleanup during reuse.

## 2026-09-29 - Route simple answers independently of mutation plans

- Type: decision
- Source: user + T057
- Confidence: high
- Review: local `task verify` and documentation integrity
- Supersedes: T050's accidental coupling to `execution.plan_mode`

Content:

Interactive execute requests may return a context-sufficient, tool-free answer
without a plan or approval even when the mutation plan gate is enabled.
`execution.plan_mode` still requires an approved persisted plan for mutations.
Only multi-step plans appear as live transcript checklists; plain chat reports
concise progress, and `/plan` remains available for a one-step plan. Live
multi-step progress comes from persisted step state rather than parsed display
text, with distinct pending, active, blocked, stopped, and completed markers.

## 2026-09-29 - Keep local preflight failures scoped to affected tools

- Type: decision
- Source: user + T058
- Confidence: high
- Review: local verification and documentation integrity
- Supersedes: T054/T056 whole-batch stop for independent repository reads

Content:

Managed worktree preparation and tool instruction scope failures are recorded as
bounded local categories. A rejected invocation receives one audited failure; eligible
project reads in the same batch may continue under their own instructions. If worktree
preparation fails, project reads use the main checkout and mutating tools remain
blocked. The read-only `git_status` tool uses the repository view selected for the
batch without creating a worktree. `nib doctor` reports stale owned session receipts
without repairing uncertain Git or filesystem state. Skill-creation answers can use
the bounded project guide in answer-only context.
