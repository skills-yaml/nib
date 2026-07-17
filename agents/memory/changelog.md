# Memory Changelog

## 2026-06-17 - Initialize Agent Memory

- Type: fact
- Source: user
- Confidence: high
- Review: none
- Supersedes: none

Content:

Initialized `agents/memory/` for nib during additive adoption of `workspace-docs@1.0.0`.

## 2026-06-20 - PR #1 Merged: Rust CLI + LLM Agent Loop

- Type: release / milestone
- Source: merge
- Confidence: high

Content:

Pull request #1 (feat/implement-basic-agent-tools) merged into main.

Major deliverables:
- Full Rust CLI port (auth, chat with /model only, run, etc.).
- LLM integration (multi-provider via LiteLLM) + core agent loop.
- Per-project .nib/ sessions and config.
- Hybrid sandbox foundations and specs FT-003 / FT-004 marked done.
- skm-style CI, installers, and Task integration.

See decisions.md for details. Merge commit: e47cb7f.

## 2026-07-02 - FT-005: Pure Rust core implemented

- Type: implementation / milestone
- Source: FT-005 Phases 0–6
- Confidence: high

Content:

- Migrated agent loop, ToolExecutor, all 5 core tools, LLM providers (OpenAI, Anthropic, Gemini, Grok, OpenRouter, Mock), context (AGENTS.md), sandbox detection, ratatui TUI stub, and `nib doctor` to Rust.
- Removed Python core (`src/nib/`, `pyproject.toml`); `nib chat` / `nib run` run in-process.
- 15 Rust tests passing; `task check` green.

## 2026-07-15 - Done-spec implementation audit

- Type: process / validation
- Source: repository audit
- Confidence: high

Content:

Audited every spec in `docs/specs/done/` against current code, tests, documentation,
and external release evidence. Unsupported completion claims were reopened in
`development/`; duplicate feature IDs were corrected; `task docs:check` was added to
prevent broken links, duplicate IDs, and unchecked done-spec acceptance items.

## 2026-07-15 - Development-spec implementation reconciliation

- Type: process / validation
- Source: repository audit
- Confidence: high

Content:

Reconciled all 25 files in `docs/specs/development/` against the Rust source and test
tree. Every spec now has explicit Development status and an authoritative scope,
acceptance, affected-area, evidence, gate, and gap section. No spec moved to `done/`.
The runtime coverage gate passed at 83.00 percent, and `task docs:check` passed all
five documentation integrity tests; aggregate repository gates remain for final
integration.

## 2026-07-15 - Done-spec implementation remediation completed

- Type: implementation / validation milestone
- Source: repository audit and canonical Task gates
- Confidence: high

Content:

Closed every feasible in-repository implementation gap found while auditing the 27
completion claims. Twenty-four specs are verified in `done/`. FT-001 and T006 remain
in `development/` until the configured Windows CI job executes successfully. T010
remains there because the exact current release-workflow revision needs a committed
development-channel run and GitHub's Release mutation API cannot fence simultaneous
external retagging; the project must either exclude external writers or adopt an
immutable, Git-CAS-controlled channel pointer. FT-015's repository merge lock now uses
a persistent `.nib` hardlink anchor, closing the last replaceable-lock-domain finding.
Current local evidence is 453 deterministic tests, a green post-transition
`task check`, 84.27 percent runtime line coverage, the locked optimized build, and
isolated release smoke for version, project-doc context, and doctor.

## 2026-07-15 - Final quality review reopened durable state and MCP lifecycle specs

- Type: implementation / security review
- Source: final two-stage review
- Confidence: high

Content:

The final spec-compliance review passed, but the subsequent code-quality/security
review found replaceable durable/daemon lock domains, a non-resumable `reconciling`
task state, MCP startup errors that could precede configured-secret redaction, and an
inbound MCP server that could not consume cancellation while a tool was active. T004,
T020, and FT-016 returned to `development/` with explicit remediation and validation
criteria. The release transaction review was clean after its backup-only recovery fix.

## 2026-07-15 - Provisional done-spec audit reconciliation

- Type: process / documentation reconciliation
- Source: repository audit
- Confidence: high

Content:

Reconciled the lifecycle inventory to 18 specs in `done/`, 9 in `development/`, and 1
in `backlog/`. This supersedes the earlier 24-done and 21-done current-state claims.
T003, T004, T007, and FT-015 now state that conditional namespace quarantine does not
prove exact Unix physical unlink. FT-017 owns stronger abrupt-owner descendant-process
containment. Historical test and coverage numbers remain labeled as historical; final
canonical Task gates, platform gates, coverage, and release smoke are still pending and
this entry does not claim a green reconciled tree.

## 2026-07-16 - Final spec reconciliation and local validation

- Type: process / validation
- Source: repository audit and canonical Task gates
- Confidence: high
- Review: none
- Supersedes: 2026-07-15 provisional reconciliation and earlier current-state gate claims

Content:

The reconciled lifecycle is 18 done, 9 development, and 1 backlog. Local stdio MCP
transport, redaction, metadata, cancellation, EOF, backpressure, and reconciliation
gaps were closed; managed-worktree cleanup now removes exact-owned artifacts and
preserves or reports unproven state.

The Linux tree passed `task check`, independent `task test` with 700 top-level tests,
85.21 percent coverage (45,586/53,499), all five `task docs:check` invariants, the
locked release build, strict format/check/Clippy/diff gates, release-binary smoke, and
raw-PTY interaction smoke. Windows and macOS runtime validation were unavailable and
remain open, along with FT-015 ownership-provenance limits and the other documented
development and backlog boundaries.

## 2026-07-16 - Post-validation audit reopened managed-process remediation

- Type: process / validation
- Source: independent spec and code-quality review
- Confidence: high
- Review: none
- Supersedes: 2026-07-16 final spec reconciliation and local validation

Content:

The current lifecycle is 18 done, 10 development, and 0 backlog. FT-017 remains in
development while process-state publication bounds, proof-bound retirement, and the
Linux namespace-root readiness and recovery boundary are reconciled. Earlier local
gate counts are historical evidence for the prior tree; canonical Task, coverage,
release-smoke, and final review gates must be rerun after this remediation.

## 2026-07-16 - Managed-process remediation revalidated locally

- Type: implementation / validation
- Source: canonical Task gates and independent two-stage review
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: 2026-07-16 post-validation audit reopened managed-process remediation

Content:

The lifecycle remains 18 done, 10 development, and 0 backlog. The Linux managed-process
remediation now includes exact gated namespace startup, proof-bound scope retirement,
runtime-drop cancellation established before task polling, bounded MCP shutdown handoff,
exact session float readback, and managed-process-only production capability preflight.
Legacy local tasks are stopped when possible without falsely terminalizing unreconciled
durable records.

The reconciled Linux tree passed `task check`, independent `task test` with 772 tests
(588 library, 53 CLI, and 131 integration), all five `task docs:check` invariants,
83.94 percent runtime line coverage (53,734/64,015), the locked optimized build, strict
host all-target/all-feature checks, and the real abrupt-owner managed-process release
smoke. Native Windows and macOS execution remains open; local cross-target checks stop
in `ring` because the MSVC librarian and Apple C toolchain/SDK are unavailable.

## 2026-07-16 - Done-spec audit completed with exact plan and audit ownership

- Type: implementation / validation
- Source: done-spec audit, remediation, and canonical Task gates
- Confidence: high
- Review: independent spec-compliance and code-quality audits
- Supersedes: 2026-07-16 managed-process remediation revalidated locally

Content:

The audit of all previously completed specs is reconciled to 18 specs in `done/`, 10
in `development/`, and none in `backlog/`. Nine reopened specs returned to `done/`
after feasible gaps were implemented; the remaining ten retain explicit remote,
Windows, macOS, or stronger platform-authority gates.

Final remediation added whole-run session leases, immutable plan identity and normalized
goal binding, strict plan-structure validation, stale-approval compare-and-set behavior,
completed-plan mutation denial, mandatory profile-scoped implicit executor audit,
authoritative-only plan audit linkage, strict skill inventory errors, shared console
question input, and a real edit/compression/`cargo test` agent-loop scenario.

The reconciled Linux tree passed `task check` and independent `task test` with 795 tests
(601 library, 61 CLI, and 133 integration), plus 83.90 percent runtime line coverage
(55,083/65,656). Final documentation, optimized-build, managed-process release smoke,
strict all-target/all-feature, formatting, and diff checks are recorded by the current
validation run.
