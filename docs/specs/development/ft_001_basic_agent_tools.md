# FT-001: Basic Agent Tools Implementation

**Status:** Development
**Owner:** nib team  
**Related:** [Product Foundation](../foundation/product.md), [Ecosystem Integration](../../tech/ecosystem_integration.md), [Permissions & Safety](../../tech/permissions.md), [Base Architecture](../../tech/architecture.md)

> Historical proposal note: the interface, Python approach, and June implementation
> snapshot below preserve the feature's original design history. The 2026-07-15 Rust
> reconciliation is the authoritative implementation and validation record.

## Overview

nib requires a small, well-defined set of core tools to function as an effective coding and workload agent. These tools enable the agent to inspect, modify, and execute work in a local development environment while maintaining strict safety, permission, and audit boundaries.

This feature defines the minimal viable tool surface, their interfaces, permission classifications, and integration points with the rest of the nib system (workload model, planner, executor, reconciler, MCP, Skills, and AGENTS.md).

The design prioritizes safety and leverage over breadth: reuse existing patterns from subagent-driven-development and the workspace conventions rather than reinventing a large tool library.

## Goals

- Provide exactly the minimal set of tools needed for high-quality coding work (read, search, edit, execute) without over-permissioning.
- Enforce a consistent, auditable permission model across all tools.
- Make every tool invocation go through the workload model (record what was done, why, and the outcome).
- Ensure tools are usable both directly by nib's core and exposable via MCP so other agents (Grok, Claude, and similar) can delegate to nib.
- Support dynamic behavior via Skills (e.g., a skill can contribute additional tool constraints or post-processing).
- Automatically respect any `AGENTS.md` / project guidelines loaded for the current context (e.g., "never run `npm install` without approval").
- Enable safe parallel execution via worktree isolation for edit/execute tools.
- Keep the surface small enough that it can be fully documented, tested, and reasoned about by both humans and sub-agents.

## Non-goals (for v1)

- Full browser automation or web interaction tools.
- Arbitrary database or cloud resource access (use MCP servers for that when needed).
- General-purpose code execution sandbox (rely on the host's normal shell + worktrees + approval). See FT-003 for direct bwrap sandboxing (with Codex patterns as reference).
- Semantic code search / embeddings (start with simple grep + read).
- Tool discovery or self-modification by the agent at runtime (static tool registry).
- Cross-project write access by default (read access to shared libs/docs is allowed via scoped context loading).

## Historical Tool Interface Proposal (Minimal Set)

All tools follow a common pattern:
- Take explicit `project_root` or `cwd` (enforces scoping).
- Return structured output (the proposal used Pydantic; the shipped Rust tools use
  bounded JSON Schema contracts).
- Log every call to the workload store with outcome, duration, and any approvals granted.
- Respect the current context's loaded AGENTS.md rules.

### 1. `read_file`
- **Purpose**: Read file contents, optionally with line range.
- **Parameters**:
  - `path`: relative or absolute (must resolve inside allowed scope)
  - `start_line`, `end_line`: optional (0-based or 1-based, consistent with conventions)
- **Permission Level**: Read-only
- **Safety**: Path scoping + secret redaction on output (configurable).
- **Output**: `{"path": "...", "content": "...", "start_line": N, "end_line": M, "truncated": bool}`

### 2. `list_directory`
- **Purpose**: List files and directories.
- **Parameters**:
  - `path`
  - `recursive`: bool (default false, with sensible depth limit)
  - `include_hidden`: bool
- **Permission Level**: Read-only
- **Output (target)**: List of entries with type (file/dir), size, mtime.  
  The 2026-06 implementation returned only `path` + `type`; the reconciliation below
  supersedes that snapshot.

### 3. `grep` / `search_files`
- **Purpose**: Search file contents or filenames.
- **Parameters**:
  - `pattern`: regex or literal
  - `path`: scope (defaults to project root)
  - `glob`: e.g. `**/*.py`
  - `max_results`: int
- **Permission Level**: Read-only
- **Output (target)**: List of matches with file, line, snippet (redacted).  
  The 2026-06 implementation performed case-insensitive substring matching; the
  shipped bounded implementation and terminal path supersede that snapshot.

### 4. `apply_patch` (preferred edit tool)
- **Purpose**: Apply a unified diff / patch safely.
- **Parameters**:
  - `path` or `worktree_id`
  - `patch`: string (unified diff)
  - `dry_run`: bool (default true for review)
- **Permission Level**: Safe write (or Destructive if it touches protected paths)
- **Safety**:
  - Must apply cleanly or fail explicitly.
  - Prefer execution inside an isolated git worktree.
  - After apply, optionally run a verification command.
- **Output**: `{"applied": bool, "hunks": [...], "conflicts": [...], "new_state": "diff"}`

### 5. `run_terminal`
- **Purpose**: Execute shell commands (the highest-risk tool).
- **Parameters**:
  - `command`: string
  - `cwd`: optional (defaults to current project/worktree)
  - `timeout`: seconds
  - `background`: bool (for long-running)
  - `worktree_id`: to force isolation
- **Permission Level**: Classified at call time (Read-only / Safe / Destructive / Network)
- **Safety** (mandatory):
  - Command classification before execution.
  - Approval workflow based on current `approvals.mode` (manual / smart / off).
  - Worktree isolation for any write or build command when possible.
  - Output streaming + redaction.
  - Hard timeout and kill handling.
  - Never execute from project root by default if a clean worktree is available for the task.
- **Output**: `{"exit_code": int, "stdout": "...", "stderr": "...", "duration": float, "approval_granted": bool}`

### Optional but recommended in v1
- `git_status`, `git_diff` (thin wrappers or restricted `run_terminal` aliases)
- `verify` (runs the project's canonical test/lint command and parses results)

## Permission & Safety Model

(See the full deep-dive document: [docs/tech/permissions.md](../../tech/permissions.md).)

Key points for tool implementation:
- All tools go through a central `ToolExecutor`.
- Classification into read-only / safe / destructive / network.
- Multiple enforcement layers (scoping, isolation/worktrees, policy, approval workflow, redaction, audit, AGENTS.md rules, skills constraints).
- Destructive actions require either real-time user approval **or** explicit prior permission (via policy, `nib allow`, or AGENTS.md allowlists).
- Every call (and its approval decision) must be recorded against the owning Task in the workload model.

Tools are classified into levels:
- Read-only
- Safe write (new files, clean patches in worktree)
- Destructive / High-risk (delete, force git, global installs, network writes)
- Network (outbound)

Enforcement layers (all must pass):
1. Path scoping (hard allowlist: current project + explicitly approved shared docs/libs roots).
2. Command / action classification.
3. Approval mode (manual prompt via TUI/CLI, smart classifier, or yolo).
4. Worktree isolation (default for edit/execute on coding tasks).
5. Audit + reconciliation (every tool call is recorded against the owning Task in the workload model).
6. AGENTS.md rules (e.g., "run `cargo test` only after `cargo check` succeeds"; "never commit directly").

Skills can add extra constraints or post-processing for specific tool calls.

MCP-exposed versions of these tools must carry the same permission metadata.

## Integration Points

### With Workload Model
- Every tool call is linked to the active Task/Project.
- Store: command/patch, approval decision, result summary, artifacts (diffs, logs).
- The reconciler uses tool history when deciding if a task is complete.

### With MCP
- All five core tools (plus verify) must be exposable as MCP tools.
- nib can act as both MCP client (consume external tools) and server (offer its tools + workload context to other agents).
- Tool calls arriving via MCP still go through the full permission pipeline.

### With Skills
- Skills can be activated per-task and influence:
  - Additional system instructions for tool use.
  - Custom tool wrappers (e.g., a "safe-rust-build" skill that wraps `run_terminal`).
  - Post-execution hooks (e.g., "after apply_patch, always run the project's test command").
- Discovery and activation happens via the context loader (already scaffolded).

### With AGENTS.md / Project Guidelines
- Context loader always injects the relevant AGENTS.md before any planning or tool-using step.
- The planner and any sub-agents must be explicitly told to follow the loaded guidelines.
- Tool usage that would violate loaded AGENTS.md should be blocked or escalated.

### With Libs Documentation (previous requirement)
- The context assembly step can safely load relevant shared libs docs and models (read-only, scoped) so the agent understands domain boundaries before using edit/execute tools.

## Historical Implementation Approach (Python Proposal)

- Tool registry in `src/nib/tools/` (or `core/tools.py` initially).
- Each tool is a Pydantic-validated function + metadata (name, description, permission_level, requires_approval, mcp_exposable).
- Central `ToolExecutor` that:
  - Resolves current scope + active AGENTS.md + skills.
  - Classifies the call.
  - Obtains approval (via TUI dialog or smart policy).
  - Executes (prefer worktree for writes).
  - Records to workload store.
  - Returns structured result (with redaction applied).
- Use `asyncio` + `subprocess` (with PTY support for interactive feel where needed) for terminal.
- For patching: use `patch` utility or Python's `difflib` + validation; always prefer git apply inside a worktree.
- MCP layer in `integrations/mcp.py` wraps the same tool functions.
- Skills integration: skills can register additional constraints or wrapper functions at activation time.

Start with pure function-based tools (easy to test). Move to class-based registry only when dynamic skill contribution is needed.

## Historical Implementation Snapshot (2026-06)

The core permission model, registry, executor, worktree support, workload audit recording, context/AGENTS/skills loading, and MCP stubs were delivered as part of this feature. See [docs/tech/architecture.md](../../tech/architecture.md) for the as-built module summary.

**Delivered:**
- Tool models (PermissionLevel, Approval*, ToolCall/Result, ToolExecutionRecord)
- Registry + 5 tools registered with metadata
- ToolExecutor with scoping, worktree auto-selection for SAFE/DESTRUCTIVE, approval modes (MANUAL default with rich CLI prompt), basic redaction placeholder, audit recording to WorkloadStore
- WorktreeManager (git worktree create/cleanup/status)
- Functional read-only tools: `read_file`, `list_directory`, `grep` (basic Python impl; grep is substring case-insensitive)
- `apply_patch` and `run_terminal`: stub implementations (return preview messages; real `git apply` in worktree + asyncio subprocess + classification/redaction/TODOs remain)
- WorkloadStore: tables + `record_tool_execution` + history query (snapshot stubbed)
- Context assembly + `nib context` command exercising AGENTS.md + skills
- `nib demo-tool` exercising the executor + permission flow + DB
- CLI surface updated for tools demo

**Gaps vs this spec (tracked for follow-up):**
- Real implementation of edit/execute tools (apply_patch using git apply; run_terminal using subprocess or Codex sandbox per FT-003)
- Full dynamic classification for run_terminal; POLICY/SMART approval modes beyond fallback
- Secret redaction, improved output formats (size/mtime on list, regex on grep)
- TUI approval flows
- Rich E2E demo performing an actual code change + reconciliation inside worktree
- `task check` (format + 90 pyright errors in executor types) and expanded tests
- Deeper Skills/MCP/AGENTS influence inside the executor decision path

The skeleton and permission layers closely match the design in this spec and the finalized architecture.md.

## Historical Acceptance Criteria (Reconciled Below)

These criteria remain the feature contract, interpreted through the current
profile-session workload model and bounded Rust schemas documented below.

- [x] All five core tools are implemented with bounded Rust/JSON Schema contracts.
- [x] Every tool call is recorded in the profile-session workload model with a full audit trail.
- [x] Path scoping and worktree isolation apply to edit/execute tools.
- [x] Manual and smart approval workflows function through CLI and TUI channels.
- [x] Tools are registered and callable directly and through stdio MCP.
- [x] Relevant skills can inject instructions and enforce tool constraints/hooks.
- [x] Loaded AGENTS.md rules are visible to the planner and can block, tighten, or require approval for tool use.
- [x] Project standards and library documentation are loaded read-only through bounded, symlink-safe context discovery.
- [x] Unit and integration tests cover every tool, denial paths, and worktree behavior.
- [x] `task check` and `task test` pass with the focused and end-to-end suites.
- [x] The coding E2E loads context, edits only a session worktree, records the call, and verifies the artifact.

**Historical snapshot note:** At the 2026-06 checkpoint, write tools, richer tests,
and final gates were incomplete. The reconciliation below records their completion.

## Historical Open Questions / Risks

- Exact surface for the patch format (unified diff only? Support for "edit by search/replace" as well?).
- How sophisticated the "smart" approval classifier should be in v1 (simple rules + optional small LLM call?).
- Token budget for injecting full AGENTS.md + multiple skills into every planning step (need summarization or selective loading strategy).
- Whether `run_terminal` should support a "safe mode" that only allows commands from an allowlist defined in the project's AGENTS.md.

---

**Historical next step (completed):** The work was decomposed across T001 and the
later sandbox, MCP, skills, planner, approval, and delegation specs.

## Reopened Audit (2026-07-15)

Scope and affected areas are implemented through T001. Completion additionally
requires every acceptance checkbox above to be backed by a focused test or runtime
artifact rather than status prose.

Validation gates: T001 gates, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

The authoritative implementation is the Rust tool registry/core/executor, profile
session audit, worktree sandbox, context policy, and stdio MCP exposure described by T001.

### Acceptance Criteria

- [x] All five core tools are real, bounded, registered, and centrally dispatched.
- [x] Scope, approved-plan, policy, worktree, approval, redaction, and audit layers are enforced.
- [x] Skills and AGENTS rules influence model context and executor decisions.
- [x] Fixed project standards/library documentation roots are loaded read-only with
  file/count/aggregate/depth bounds, deterministic ordering, and symlink rejection.
- [x] Direct and stdio MCP invocation paths use the same executor.
- [x] A verified E2E patch changes only the session worktree.
- [x] Fresh local aggregate gates are green.
- [ ] Windows runtime gates are green.

### Affected Areas

`src/tools/`, `src/sandbox/`, `src/agent/`, `src/context/`, `src/session/`,
`src/integrations/`, and core-tool E2E tests.

### Implementation Evidence

`src/tools/registry.rs`, `src/tools/core.rs`, and `src/tools/executor.rs` replace the
historical Python/Pydantic proposal. `src/agent/loop.rs` links observations to the
profile session and persisted `PlanStep` rather than a global Task row.
`src/context/project_docs.rs` discovers bounded conventional standards and library
documentation, and `src/context/budget.rs` accounts for that group in the aggregate
planner/runtime envelope.

### Validation Evidence

`tests/executor.rs` covers scope, plan gates, policy, failures, memory, and background
delivery. `tests/test_runtime_e2e.rs` covers skill denial, physical patch verification,
real artifacts/errors, and permission-gated stdio MCP delegation.
`src/context/project_docs.rs` tests deterministic discovery, per-file/aggregate/count
bounds, and symlink escapes; context/planner budget tests prove bounded injection.

### Historical Validation Gates

These checked results describe the earlier reconciliation snapshot. The later
Windows/filesystem remediation gates below are authoritative for completion.

- [x] Focused and E2E evidence named in T001 exists.
- [x] `task check`.
- [x] `task test`.

### Superseded Gap Assessment

Global Task-ID attachment is superseded by profile session, plan, and durable-task
ownership. The statement that no in-scope gap remained is superseded by the
Windows/filesystem remediation below.

## Final Quality Review Remediation (2026-07-15)

### Scope

Apply the advertised discovery-entry bound before allocating and sorting project
documentation directory contents, instrument the retained heap so the regression
observes its peak storage directly, and prove opened document identity on every
supported platform in CI. Treat Windows canonical verbatim-path syntax and DOS 8.3
short aliases as the same lexical path without weakening component-level symlink or
reparse rejection, skipping the second component-validation pass, or changing the
canonical path returned to callers.

### Acceptance Criteria

- [x] A directory with more than the discovery cap cannot allocate an unbounded entry vector.
- [x] A test-only observer proves the retained directory-entry heap never grows beyond
  the configured discovery cap while every overflow entry is considered.
- [x] Selection remains deterministic within the global entry, depth, file, and byte caps.
- [x] File replacement between path validation and read cannot bypass identity checks on
  the local Unix host; unsupported local identity semantics fail closed.
- [x] Windows file replacement and reparse runtime regressions prove the same guarantee.
- [x] Windows `\\?\` canonical prefixes and DOS 8.3 short aliases do not make ordinary
  absolute directories fail validation, while every path component is checked twice
  for symlinks and every other reparse-point type before acceptance.
- [x] CI executes the Rust test suite on Windows so the Windows replacement regression is
  compiled and run rather than only cross-compiled by the release workflow.
- [x] Overflow and symlink regressions pass on the local Unix validation host.

### Affected Areas

`src/context/project_docs.rs`, `src/fs_security.rs`, project-context/filesystem tests,
and `.github/workflows/ci.yml`.

### Validation Gates

Focused project-doc tests including observed peak heap storage, real-directory verbatim-
prefix validation, reparse-point rejection, `task test`, `task check`, `task coverage`,
and a Windows CI `task test` job that executes the Windows file-identity and path regressions.

### Validation Evidence

`bounds_directory_selection_before_sorting_overflow_entries` observes every overflow
entry while proving retained heap storage never exceeds `MAX_DISCOVERY_ENTRIES` and
selection remains deterministic. `ignores_project_local_symlinked_conventional_root_ancestor`
proves a project-local `docs` symlink is rejected even when its target remains inside
the project, and bounded reads revalidate document parents around file identity checks.
Directory entries plus the pre/post metadata for every accepted document use the shared
symlink-or-reparse predicate; the focused project-document suite passes 6/6 locally.
Local filesystem component-link regressions pass. Windows lexical comparison now uses
`GetLongPathNameW` only after the first component check, repeats the component check and
canonicalization, and includes a real `GetShortPathNameW` alias regression that skips
only when the volume cannot produce a distinct short path. Fresh local Task gates execute 772
tests, coverage passes at 83.94 percent (53,734/64,015), and the locked build plus Linux
release/PTY smoke pass on 2026-07-16. Hosted Windows run `29842405062` executes and
passes `windows_file_replacement_cannot_bypass_opened_identity_check`, the directory and
nested-junction reparse regressions, the real DOS short-alias regression, and both
verbatim-prefix regressions.

## Remaining Implementation Plan

1. Complete the hosted Windows core-tool portability follow-up below and pass the exact
   revision's full CI matrix.
2. Keep the native file-identity, reparse-point, verbatim-path, and DOS short-alias
   regressions mandatory for future filesystem changes.
3. Rerun the canonical Task gates and two-stage review before moving FT-001 to `done/`.

## Current Risks

- A future path-normalization change could weaken component-level link rejection unless
  the passing native Windows replacement and reparse regressions remain mandatory.
- The hosted terminal linker and full-matrix risks remain open in the portability
  follow-up below.

## Hosted Windows Core-Tool Portability Follow-up (2026-07-21)

### Scope

Repair the native Windows failures exposed after the MCP lifecycle and session-lock
suites completed. Keep `list_directory` and `grep` path values in their existing native
filesystem syntax, but make the E2E assertions compare them as paths instead of Unix
strings and normalize only relative glob candidates to slash syntax. Preserve the
machine-wide `ProgramData`, `ProgramFiles(x86)`, and `ProgramFiles` discovery roots
across the sanitized terminal environment so rustc can inspect Visual Studio Setup
Configuration state, fall back to the fixed `vswhere` location when necessary, select
the absolute MSVC linker, and build its SDK environment instead of falling through to
Git's GNU `link.exe`. Keep the POSIX shell selection and real Cargo-test verification
unchanged.

### Acceptance Criteria

- [x] Core-tool E2E path assertions accept native Windows separators without weakening
  the recursive listing, hidden-entry, glob, or concrete artifact checks.
- [x] Slash-based grep globs match native Windows relative paths while returned file
  paths remain native and usable as filesystem inputs.
- [ ] Sanitized Windows terminal children retain the three machine-wide Visual Studio
  discovery roots needed by rustc through Git Bash, while unrelated host variables
  remain excluded.
- [x] POSIX shell selection and the existing environment redaction boundary remain
  otherwise unchanged.
- [ ] The coding E2E still applies its patch only in the session worktree, runs the real
  fixture `cargo test`, and persists a successful terminal result on Windows.
- [ ] The exact PR revision passes the hosted Windows job and full CI matrix.

### Affected Areas

`src/sandbox/mod.rs`, `src/tools/core.rs`, `tests/test_runtime_e2e.rs`, focused sandbox
environment tests, this FT-001 evidence, and the hosted Windows job.

### Validation Gates

Focused sandbox and runtime E2E tests, `task fix`, `task test`, `task check`,
`task docs:check`, Windows-target `task check:all-targets`, `git diff --check`, and the
exact-revision hosted Linux/macOS/Windows matrix.

### Reproduction Evidence

Hosted run `29832617613` on revision `2cf7e584b0ac71182d134582dfc875339376770c`
passed Validate, macOS, every MCP integration test, and the Windows session-roundtrip
suite. Windows then exposed two FT-001 failures in `tests/test_runtime_e2e.rs`:
`list_directory` returned the native `nested\\lib.rs` path while the assertion required
`nested/lib.rs`, and Git for Windows' GNU `link.exe` consumed rustc's MSVC response file
during the real Cargo-test step. The latter exited 101 with `link: missing operand`, so
the persisted terminal result correctly recorded failure.

### Local Validation Evidence

On 2026-07-21, the focused slash-glob regression and both formerly failing runtime E2E
tests pass locally, including the nested fixture's real `cargo test`. `task fix`,
`task test`, `task check`, `task docs:check`, `git diff --check`, and
`task check:all-targets TARGET=x86_64-pc-windows-msvc` pass. The Windows-only
environment regression compiles for MSVC and remains unchecked above until the hosted
runner executes it together with the native path and terminal behavior.

### First Hosted Remediation Evidence

Hosted run `29836694411` on revision `29dc8af8c83b6dd9c72379e8b7377adf1ae5e009`
passed Validate and macOS. Windows passed the focused environment unit test and eight
of nine runtime E2Es, including the complete native path, hidden-entry, and slash-glob
artifact assertions. The unchanged real Cargo-test E2E still invoked logical
`link.exe`, which Git Bash resolved to Git's GNU linker, and exited 101.

That run disproves the earlier assumption that retaining only `ProgramFiles(x86)` and
`ProgramFiles` was sufficient. Rust's MSVC discovery first enumerates Visual Studio
Setup Configuration state, whose machine-wide implementation and instance records are
under `ProgramData`; merely making the `vswhere` executable reachable does not prove
that enumeration reaches its fallback. The next hosted gate must exercise a real rustc
link through the sanitized Git Bash process chain before the environment criterion can
be checked.

### Second Hosted Remediation Evidence

Hosted run `29842405062` on revision `3ec0fbb4859dea143a34fc219309a1c88c1b3179`
passed Validate, macOS, the Windows all-targets check, and 546 of 547 Windows library
tests. Its new linker probe failed in 0.14 seconds with empty output before invoking
rustc, so the original coding E2E did not run and this revision neither confirmed nor
disproved the `ProgramData` remediation.

Git for Windows canonicalizes the native `ProgramFiles` key to `PROGRAMFILES` while
importing it into the case-sensitive POSIX shell. The probe incorrectly checked the
mixed-case spelling in a silent `&&` chain. The next revision must use the shell-visible
spelling, emit a named error for every environment precondition, include the process
status in assertion output, and retain the real rustc plus Cargo linking gates.
