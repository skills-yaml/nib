# T080: Claude Code / Codex-Style Workspace, Sandbox and Permission Modes

**Status:** Development. Implementation started on 2026-10-08 with phase 1, after the
user requested implementation.
State: development
Primary Feature: tools-sandbox

## Problem and Authority

The user asked on 2026-10-07 for nib to have "the same user experience as
Claude Code or Codex" and for the spec to combine the best of both. This
supersedes the earlier read-only-project draft of T080. Its findings remain
requirements:

- **Git fails in every session.** Sessions always run in a linked worktree
  (`src/agent/loop/inner_exec.rs:92-98`). The bwrap boundary hides `$HOME`,
  which includes the common `.git` (`src/sandbox/mod.rs:1073-1101`), so
  `git_status` and `run_terminal` git fail with `not a git repository`. This
  was reproduced with session `21914944`.
- **Secrets are exposed.** Tool commands bind their working directory over a
  read-only `/` and mask `$HOME` only when the project is under it. A command
  whose working directory is the project root, or a project outside `$HOME`,
  can read `<project>/.nib/config.toml`, which holds the plaintext `api_key`.
  - *Correction (2026-10-08):* the original draft also named the
    supervised-process launcher (`src/sandbox/process/split_01.rs:659-678`).
    That launcher runs nib's own `subagent-worker` in a PID namespace for
    cleanup. The worker legitimately needs provider credentials and network
    access, and its tool commands already go through the shared tool sandbox.
    The fix therefore belongs in the tool-command mount plan, not the
    launcher.
- **Denials are silent.** `approvals.mode = "policy"` denies unmatched commands
  without asking the user. In session `21914944`, a read-only
  `git status; git log` was classified destructive and denied.
- **The mode is invisible and fixed.** Users cannot see or switch how much
  autonomy the agent has during a session.
- **Work is hidden.** Edits land in a hidden worktree instead of the user's
  checkout, and sessions have no way to land their work.

## Design: Best of Both

| Concern | Taken from | nib behavior |
| --- | --- | --- |
| Where work happens | both | By default, in the user's folder, directly. Worktrees are opt-in. |
| Sandbox levels | Codex `--sandbox` | `read-only`, `workspace-write` (default), `full-access` |
| Autonomy modes | Claude Code permission modes | `plan`, `ask` (default), `accept-edits`, `auto`; Shift+Tab cycles them |
| Rule-based permissions | Claude Code allow/ask/deny rules | Layered rules by tool and command prefix or path; deny wins |
| Escalation on sandbox block | Codex on-failure | Offer "retry outside sandbox?" in a prompt; never silent |
| Edit safety net | Claude Code checkpoints / rewind | Every agent edit is checkpointed; `/rewind` restores files |
| Repository trust | both (trust prompt) | Project rules and hooks apply only after the user trusts the folder |
| Headless use | Codex `exec` | `nib run --sandbox <level> --mode <mode>` with the same semantics |
| Visibility | both | The status line always shows mode, sandbox and folder or worktree |

### 1. Workspace

- **Default: in place.** The agent works in the folder where nib was started.
  Edits appear immediately in the user's editor and `git status`. Several
  sessions may share a folder (section 6).
- **Opt-in worktree.** `/worktree`, `--worktree` or `execution.workspace =
  "worktree"` runs the session in a managed worktree on `nib/session/<id>`, for
  parallel or background work. Subagents and scheduled tasks keep using
  worktrees. When work in a worktree is done, `/land` previews the diff, then
  commits to the session branch, merges it into the user's branch, or
  discards it, each with a prompt.

### 2. Sandbox levels (Linux bwrap; macOS and Windows fail closed or ask)

| Level | Filesystem | Network |
| --- | --- | --- |
| `read-only` | Whole project readable. No writes except a private temp directory. | off |
| `workspace-write` (default) | Workspace writable. All Git metadata (`.git` and a worktree's `.git` pointer) is read-only; Git writes go through host-side tools (section 7). `.nib/` is hidden. | on (user decision D4) |
| `full-access` | No sandbox, as with current `internal` direct execution. Requires an explicit choice and shows a warning badge. | on |

Rules for every level:

- `$HOME` is masked except the workspace path, the toolchains already mounted
  (cargo, rustup, task) and explicitly configured extra paths.
- `.nib` runtime state and credentials are never visible (tmpfs mask).
- **Git metadata is read-only in every level.** The common `.git` and, in a
  managed worktree, its `.git` pointer file are bound read-only onto
  themselves after every writable bind. Git reads (`status`, `log`, `diff`,
  `show`, `blame`) work. Git writes (`add`, `commit`, `checkout`, `stash`,
  `config`, `submodule update`) fail inside the sandbox and go through the
  approved host-side Git tools of section 7 (phase 2).

  *Revision 2026-10-08, from independent review of `ab46461`:* the first
  candidate made `.git` writable and re-mounted a block-list of executable
  surfaces read-only (hooks, config, info, modules, `core.hooksPath`,
  includes). The reviewer demonstrated host code execution through
  `commondir` redirection to an attacker repository with an `fsmonitor` hook,
  and showed that renaming a protected path's parent (`mv .git .git-old`, or
  `mv .husky`) defeated every read-only re-mount. Any writable `.git` also let
  sessions move the user's branches and stage into the main checkout's index.
  A block-list cannot make a writable `.git` safe, so Git metadata is now
  entirely read-only, and every protected path is its own mount point so it
  cannot be renamed away.
- **Residual risk, accepted and documented.** Tracked files that tools later
  execute stay editable, as in Claude Code and Codex: `Taskfile.yml`,
  `Makefile`, `.envrc`, husky's tracked `.husky/*` hook scripts, pre-commit's
  `.pre-commit-config.yaml` with `language: system`, `lefthook.yml`, and
  configuration files that the user's own Git configuration includes from the
  workspace. The mitigations are `ask` mode, diffs and the visible mode
  badge. The sandbox does not mitigate them.
- Every tool-command sandbox, including commands run by subagent workers,
  uses one mount plan (`src/sandbox/project_mounts.rs`). Its order is fixed
  and tested:
  1. home mask;
  2. read-only project;
  3. workspace (the working directory in the main checkout, or the whole
     managed worktree);
  4. configured `allow_write`;
  5. `.nib` state masks;
  6. the managed worktree re-bound inside the mask;
  7. read-only `.git` and `.git` pointer mounts.

  Masks and Git protections come after `allow_write`, so no configured path
  can expose nib state or make Git metadata writable.
- **Further rules from re-review of `872e6cd`:**
  - An `allow_write` entry that is `$HOME` or one of its ancestors is
    rejected, because it would replace the home mask.
  - Every `.nib` directory among the working directory's ancestors is
    masked, including a project outside `$HOME` reached from a nested
    repository.
  - In the fallback plan (no trusted project), a `.git` entry at the working
    directory, such as an unmanaged linked worktree, submodule or separate
    Git directory, is bound read-only and a symlink fails closed.
  - Read-only Git status works from a subdirectory of a managed worktree.
- **Further rules from re-review of `191536b`:**
  - The fallback plan re-binds its working directory after the ancestor
    masks. Session worktrees of a project whose own `.git` is a file stay
    usable.
  - Existing nested `.nib` directories (masked) and `.git` entries (bound
    read-only) are protected below the project and workspace. The scan is
    bounded: depth 3, at most 4096 directories, and it skips `node_modules`,
    `target` and `.venv`.
  - Ancestor `.nib` entries inside the masked `$HOME` and outside the bound
    area are skipped, because they are already invisible.
  - Credential masks are applied after `allow_write`.
  - Re-review of `492e42a`: credential masks are applied after the plan's
    workspace re-binds. Every directory between a writable area and a nested
    repository root is bound onto itself, so the repository cannot be renamed
    away and recreated. `allow_write` areas are scanned and protected like
    the workspace.
  - An `allow_write` path inside a `.nib` directory that contains the
    workspace stays hidden, for example a sibling session under
    `<root>/.nib/worktrees`. This fails safe.
  - Accepted residual risk: repositories or `.nib` directories that the
    sandbox creates itself, nested ones beyond the scan bounds, and symlinked
    nested `.nib` directories are not protected. Repositories the agent
    creates carry agent-authored configuration, the same trust class as
    other tracked files the user later runs.
- **Visible to the sandbox by design:** a managed session can read the main
  checkout, including untracked files such as `.env` and any credentials
  embedded in `.git/config` remote URLs. Projects that keep secrets in the
  checkout should rely on `ask` mode, or on `read-only` with `network = "off"`
  (D4 mitigations).
- **Trusted mount sources.** Sources are derived only from the canonical
  working directory and nib's fixed layout:
  - the nearest ancestor with a real `.git` directory; or
  - for `<project>/.nib/worktrees/<kind>/<name>`, that project.

  They are never derived from the sandbox-writable `.git` pointer file. Unmanaged
  linked worktrees, `$HOME` itself and its ancestors are never mounted as
  projects. A working directory inside nib state, a symlinked `.nib` or a
  symlinked Git metadata path fails closed. No host paths are created by the
  plan.
- `git_status` uses the same plan in read-only form.

### 3. Permission modes

| Mode | File edits | Commands inside sandbox | Escalation and outside-sandbox actions |
| --- | --- | --- | --- |
| `plan` | none (read-only tools; produces a plan for approval) | read-only only | none |
| `ask` (default) | prompt | prompt unless allowed by a rule or known read-only | prompt |
| `accept-edits` | automatic, checkpointed | prompt unless allowed | prompt |
| `auto` | automatic | automatic | prompt; never silent |

- Shift+Tab cycles `ask → accept-edits → plan`. `/mode <name>` sets any mode.
  `auto` requires an explicit `/mode auto` or `--mode auto`.
- Existing settings map as follows: `manual → ask`, `smart → accept-edits`,
  `off → auto`. `policy` becomes rules with fallback `ask` instead of silent
  denial; doctor reports the migration.
- `git push`, branch deletion, history rewriting and `rm -rf` outside the
  workspace always prompt, in every mode, unless the user has written an
  explicit allow rule.

### 4. Rules and prompts

- **Rule syntax:** `allow`, `ask` and `deny` lists, keyed by tool and pattern,
  for example `terminal(git status*)`, `terminal(cargo test*)`,
  `edit(src/**)` and `read(.env)`. Precedence is deny over ask over allow.
  Specific rules beat general ones.
- **Rule layers:** user (`~/.nib/config.toml`), then trusted project shared
  (`.nib/config.toml`), then project local (user-home per-project state, see
  [T082](../../backlog/architecture/T082_user_home_runtime_state_and_sqlite_sessions.md)).
  Project-shared rules apply only after the folder is trusted. Deny rules
  always apply.
- **Prompt choices:** *Yes once* / *Yes, always allow `<prefix>` in this
  project* / *No, and tell nib what to do instead*. This builds on the existing
  remembered command-prefix cards (`src/interaction_card.rs`). Compound
  commands are split and judged part by part, so a read-only `git status;
  git log` is not classified destructive.
- **Engine integration.** Rules compile into the existing policy evaluation
  (`ToolExecutor::matching_policy_rules`,
  `src/tools/executor/split_01.rs:1431`). Remembered command prefixes
  (`source: "command_prefix"`, `:1326`) become project-local allow rules
  with no loss. Skill policy rules keep their precedence below user deny
  rules.
- **Built-in read-only allowlist:** `ls`, `cat`, `rg`, `git status/log/diff/show`
  and similar run without prompting in every mode.

### 5. Checkpoints and rewind

Before each agent edit batch, nib snapshots the touched files, including new
and deleted files. Until T082 lands, snapshots are stored under the profile
state directory (`.nib/profiles/<id>/checkpoints/<session>/`, hidden from the
sandbox). T082 moves them to user-home state. There is no ordering dependency. `/rewind` lists the
checkpoints and restores the chosen one. A checkpoint covers file tools and
best-effort terminal changes (the workspace diff between turns). Checkpoints
are not git commits and never touch `.git`.

### 6. Multiple sessions in one folder (Claude Code model, user decision D5)

There is no folder lock. Any number of sessions may work in the same folder,
as in Claude Code. Conflicts are handled per file, and real parallel work uses
worktrees.

- **Stale-write check (read before edit).** `read_file` records, for each
  session, the content hash of every file it returns. `apply_patch` (nib's only
  file-editing tool, `src/tools/registry.rs:94`) succeeds only when:
  - the target file was read in this session; and
  - its current hash still equals the recorded hash.

  If the file changed on disk after the read (another session, the user, a
  formatter), the patch fails with
  `file changed since you read it; re-read <path> and retry`, and nothing is
  written. New files must not already exist. After a successful patch, the
  session's recorded hash becomes the new content, so consecutive edits do
  not need a re-read.
- **Terminal commands are not covered.** As with Claude Code's shell, a
  command that writes files (a formatter, codegen, `sed -i`) is not
  stale-checked. Its effects show up in the next read and in the run's
  changed-path record.
- **Visibility (nib addition).** The status line shows "N other sessions in
  this folder" when more than one nib session is active there, so the user
  knows edits may interleave. It is informational only and blocks nothing.
  Presence is a per-session heartbeat file in profile state, ignored after 2
  minutes without an update.
- **Parallel work.** `/worktree` gives a session its own checkout, as with
  Claude Code worktrees. Subagents and scheduled work always use worktrees.

### 6b. In-place workload reconciliation

Without a session branch, run and plan evidence binds to the existing
verification content identity (`src/agent/loop/verify.rs`).

- **Scoped to the session's own files.** In place, the identity covers only
  the paths this session changed (file tools plus the terminal-observed diff),
  not the whole folder. Another session's edits elsewhere therefore do not
  invalidate this session's verification. Each run records the workspace
  root, its checkpoint id before the run, the changed paths and the content
  identity after the run.
- **Invalidation.** Any later change to one of those paths, by anyone,
  invalidates the verification, as in worktree mode today.
- **Accepted limit.** A passing test in place may depend on another session's
  unverified edits to other files. The run record lists which other sessions
  were active, and the user guide explains when to use `/worktree` for
  isolation.

### 7. Branch, commit and worktree rules (Claude Code model, user decision D6)

Preference order: current checkout and branch, then a new branch, then a new
worktree.

1. **Default: current folder and current branch.** nib edits files where it was
   started, on whatever branch is checked out. Editing never creates a branch.
   Edits stay uncommitted until the user asks to commit.
2. **No commit or push unless the user asks.** nib never commits on its own
   initiative, including at the end of a plan. A plan step may propose
   "commit", but it runs only after an explicit request or approval. In `ask`
   and `accept-edits` modes, a commit always prompts and shows the diff stat
   and message. `git push` always prompts, in every mode.
3. **Branch first on the default or a protected branch.** When asked to commit
   while on the default branch (resolved from `origin/HEAD`, falling back to
   `main` then `master`) or a branch in `git.protected_branches` (default
   `["main", "master"]`), nib first creates and switches to
   `nib/<short-topic>`. The topic is a slug of the request or plan goal, with
   a numeric suffix on collision. nib reports the branch name. On any other
   branch, it commits there.
4. **Worktree only when isolation is needed:**
   - the user asks (`/worktree`, `--worktree`, `execution.workspace =
     "worktree"`);
   - subagents and scheduled or background work, as today;
   - project instructions (`AGENTS.md` and the other instruction files nib
     already loads) require isolation, for example "keep the primary checkout
     coordination-only during concurrent edits". nib states the reason when it
     creates the worktree.

   Worktree sessions keep `nib/session/<id>` and `/land`.
5. **Commits and pushes run on the host.** New tools `git_commit` and
   `git_push` run after approval and outside the sandbox, so GPG/SSH signing,
   credential helpers and the user's own hooks work exactly as when the user
   commits. This is safe because phase 1 makes the executable Git surfaces
   read-only to the agent, so the hooks and config that run are the user's.
   `git_commit` accepts only a message and an optional path list. Flags such
   as `--no-verify`, `--amend` and `-c` overrides are rejected; amend requires
   an explicit user request and its own prompt. A `git commit` or `git push`
   typed in `run_terminal` is redirected to these tools with an explanation.
6. **Explicit instructions win.** User requests and project instruction files
   override these defaults (for example "commit directly to main" or "always
   use a worktree").

### 8. Visibility and headless use

- **Status line:** shows `mode · sandbox · in-place|worktree:<branch>`.
- **`/status`** shows the effective rules and their source layer.
- **`nib doctor`** reports the mode, sandbox level and whether protected mounts
  are enforceable on the platform.
- **`nib run`** accepts `--mode` and `--sandbox`. Headless runs never prompt:
  an unanswerable prompt is a denial with a clear message.

## Exclusions and Compatibility

- No change to provider, LLM or plan semantics. T081 owns interrupted-plan
  behavior.
- Workload reconciliation still verifies results. In place, its evidence is
  the checkpoint diff instead of the worktree branch.
- macOS sandboxing (seatbelt) and Windows sandboxing are follow-ups. Until
  then those platforms offer only `read-only` (tool-level enforcement) and
  `full-access`, with a warning.
- Existing managed worktrees and receipts remain valid for worktree mode.

## Affected Areas

- `src/sandbox/mod.rs` and the new `src/sandbox/project_mounts.rs` (one mount
  plan) with `src/sandbox/project_mounts_tests.rs`.
- `src/tools/executor/` (approval modes, rules, prompts, compound-command
  classification).
- `src/agent/loop/inner_exec.rs` (in-place versus worktree workspace).
- `src/integrations/worktree.rs` (opt-in, `/land`).
- `src/interaction_card.rs`, `src/interactive/` and `src/tui/` (Shift+Tab,
  `/mode`, `/status`, `/rewind`, `/worktree`, `/land`, status line).
- `src/config/` (modes, sandbox, rules, migration of `approvals.mode`).
- `src/run.rs` (headless flags) and `src/doctor.rs`.
- `src/tools/registry.rs` and `src/tools/core.rs` (`git_commit`, `git_push`,
  redirecting `run_terminal` git writes) and `src/context/agents.rs`
  (instruction-driven worktree requirement).
- Tests, the user guide, the catalog, versions and memory.

## Implementation Plan (Phased Delivery)

Checkpoints come before the in-place default (user decision D2).

1. **Security and git.** Shared mount plan, the `workspace-write` default
   with protected executable Git surfaces and a masked `.nib`, the same plan
   for subagent tool commands, and `git_status` on the plan in read-only
   form. Worktrees remain the
   default.
2. **Permission modes and Git rules.** `ask` default, Shift+Tab, `/mode`, the
   status line, migration of `approvals.mode`, and the host-side `git_commit`
   and `git_push` tools with the branch-first rule (D6).
3. **Rules and prompts.** Rule layers, trust, "always allow" persistence and
   compound-command splitting.
4. **Checkpoints and `/rewind`.**
5. **In-place workspace by default.** Stale-write check, session presence,
   path-scoped in-place reconciliation, worktree opt-in and `/land`. The
   stale-write check may ship earlier, with phase 3, because it also protects
   against the user's own concurrent edits. This phase includes the approved
   governed-instruction amendments (D2).
6. **Escalation on sandbox block and headless flags.**

Each phase is independently reviewable and keeps `task verify` green.

## Acceptance Criteria

- [ ] AC-1: In `workspace-write`, Git read commands (`status`, `log`, `diff`)
  work in place and in managed worktrees under `$HOME`. Every write to Git
  metadata fails with a read-only error: hooks, config, `commondir`, refs,
  rebase state, the worktree `.git` pointer, and renaming or removing `.git`.
  Git writes are available only through the phase 2 host-side tools. `<project>/.nib` runtime and credential files are
  unreadable from every tool-command sandbox. `$HOME` stays masked.
- [ ] AC-2: A rewritten `.git` pointer, or a symlink in the workspace,
  cannot expose paths outside the mount plan (adversarial fixtures).
- [ ] AC-3: A default session edits the user's folder directly. Several
  sessions may share a folder. `/worktree` sessions and subagents stay
  isolated, and `/land` commits or merges only after confirmation.
- [ ] AC-4: Each mode enforces the matrix in section 3. Shift+Tab cycles modes
  and the status line reflects the change within the same turn. `auto` never
  silently performs an outside-sandbox action.
- [ ] AC-5: Rules follow their precedence and layering, untrusted project rules
  are ignored, and deny always wins. "Always allow" persists per project.
  `git status; git log` runs without prompting.
- [ ] AC-6: `/rewind` restores file contents, creations and deletions to a
  chosen checkpoint, and leaves `.git` untouched.
- [ ] AC-6b: In place, each run records its checkpoint, changed paths, the
  other active sessions and a content identity scoped to its own changed
  paths. A later change to one of those paths invalidates the verification.
  Another session's edits to other files do not.
- [ ] AC-6d: `apply_patch` fails, writing nothing, when the target was not read
  in the session or changed on disk after the read. Consecutive edits by the
  same session do not need a re-read. Two sessions editing the same file in
  place cannot silently overwrite each other.
- [ ] AC-6e: With two nib sessions active in one folder, each status line shows
  the other. A crashed session disappears after the heartbeat timeout.
- [ ] AC-6c: `allow_write` entries that cover the project or `.git` cannot
  expose `.nib` or make Git metadata writable. Entries covering `$HOME` are
  rejected. Ancestor `.nib` directories stay hidden from nested repositories,
  and unmanaged worktree pointers stay read-only.
- [ ] AC-6f: Editing never creates a branch or commit. A commit happens only
  after an explicit request or approval. On the default or a protected branch,
  a `nib/<topic>` branch is created first and reported. On another branch, the
  commit lands there. Push always prompts.
- [ ] AC-6g: `git_commit` and `git_push` run on the host with the user's
  signing, credential helpers and hooks. Flags that bypass hooks or rewrite
  history are rejected unless the user explicitly requested them. `git
  commit` and `git push` in `run_terminal` are redirected.
- [ ] AC-6h: A worktree is created only by user request, for subagents and
  scheduled work, or when project instructions require isolation, and nib
  states the reason. Explicit user or project instructions override the
  defaults.
- [ ] AC-7: A command blocked by the sandbox offers a retry outside it through a
  prompt. Headless runs deny instead of hanging.
- [ ] AC-8: Existing `approvals.mode` values migrate with a doctor notice.
  Governed instructions are amended exactly as scoped by D2, and only in
  phase 5.
  T063's `git_status` guarantees (no fsmonitor, no hooks, no network) still
  hold.
- [ ] AC-9: An independent exact-candidate security review, the native Linux
  bwrap fixtures (`NIB_REQUIRE_BWRAP_TESTS=1`), `task verify`,
  `task docs:check` and `task versions:check` pass. Guide, catalog and memory
  are reconciled.

## Validation Gates

For each phase: focused sandbox, executor, interactive and TUI tests plus
`task check`. Before delivery: `task verify`, `task docs:check`,
`task versions:check`, and native Linux, macOS and Windows CI. An independent
security review of the mount plan and permission engine is mandatory.

## Risks and Rollback

- **Working in place removes worktree isolation, and shared folders allow
  interleaved edits.** Mitigated by checkpoints, the per-file stale-write
  check, the presence notice, the default `ask` mode and the visible mode
  badge. `/worktree` remains available for isolation.
- **Wrong mount order could expose secrets or hooks.** Mitigated by one shared
  plan builder with order assertions and adversarial tests.
- **Rule engine bugs could over-allow.** Mitigated by deny-wins evaluation and
  property tests for precedence.
- **Rollback.** Setting `execution.workspace = "worktree"` and
  `approvals.mode = "manual"` restores the previous behavior. Phase 1 must not
  be rolled back to an unmasked state.

## Phase 2 Delivery Split (2026-10-09)

Phase 2 is delivered in two pull requests.

**2a: permission modes.** `ApprovalMode` gains `Plan`, and the names `ask`,
`accept-edits`, `plan` and `auto` are accepted alongside the original config
names. The behavior:
- `accept-edits` grants `apply_patch` automatically.
- `plan` refuses every non-read-only action before allow rules or remembered
  grants apply.
- `policy` prompts for unmatched actions only when the approval handler can
  prompt (TUI and plain chat); headless handlers still deny.
- A session `permission_mode` field, set by `/mode` or Shift+Tab, overrides
  `approvals.mode` from the next run.
- The footer shows `mode <name>`.
- Read-only command sequences and pipelines are classified read-only.
- A lone `&` is now treated as shell composition. This closes a
  classifier bypass where `git status & touch x` was approved as read-only.

**2b: host-side Git.** `git_commit`/`git_push` with approval, the branch-first
rule and the redirection of `run_terminal` Git writes.

Independent review of `39daf74` (approved with fixes), and the
resolutions:
- **H1.** Plan mode refused `ask_question`, discarding answers the user had
  already given. Plan mode now allows read-only tools, plan writing and
  questions (`plan_mode_refuses`).
- **M1.** The legacy `smart` preset keeps its previous behavior, which was
  identical to `manual`, so existing configs do not start auto-applying
  edits. Only the explicit `accept-edits` applies edits automatically.
- **M2.** During an active run, the footer shows a changed mode as
  `<mode> (next request)`.
- **L1.** The approval pre-check applies the plan refusal before
  RequireApproval rules.
- **L3.** Forks keep the session mode, except `auto`.
- **L4.** Tests prove that plan mode beats allow rules, remembered grants,
  `--yes` and classifier auto-approval, and that the pre-check matches each
  mode.
- **L2, accepted.** `/permissions` still reports only the configured preset;
  `/mode` reports the session mode.
- **Follow-up candidate (pre-existing classifier issues).** Abbreviated long
  options (`git log --outp=x`), `--ext-diff`/`--textconv`, glob-expanded
  `--output=` file names and `wc --files0-from` can make single-command
  "read-only" Git or `wc` invocations write files or run configured helpers.

**2b implementation (2026-10-09).** The host-side Git tools live in
`src/tools/git_tools.rs`.

`git_commit`:
- Arguments are a message and optional workspace-relative paths, validated
  with literal pathspecs and a `--` separator. Nothing staged is an error.
- It branches first to `nib/<slug>` (with a numeric suffix on collision)
  when HEAD is detached or on `main`, `master` or `origin/HEAD`'s branch.
- It runs on the host with the user's Git environment, with
  `GIT_TERMINAL_PROMPT=0`.
- It is level Destructive, so the mode decides: `ask` and `accept-edits`
  prompt, `plan` refuses, `auto` grants.

`git_push`:
- The remote must exist (default `origin`), and HEAD must be on a branch.
  There is no force option, and the refspec is pinned to the same branch.
- It always prompts, even in `auto`, with `--yes` or with allow rules.
  Headless runs deny.

Review of `1db1a28` (approved with fixes), and the resolutions:
- **F1.** Every commit excludes `.nib` through an `:(exclude,top).nib`
  pathspec. Explicit paths may not name nib state. Untracked nested
  repositories are refused.
- **F2.** Approval prompts carry a redacted preview: for a commit, the
  message, the branch (or the new `nib/<topic>`), the changes and the diff
  stat; for a push, the remote with credentials stripped from its URL, the
  branch and the commits to be published.
- **F3.** Allow rules can come from workspace instruction files that an
  agent could edit, so `git_push` ignores allow rules entirely. This
  supersedes D6's "explicit allow rule" exception.
- **F4.** Git runs in its own session without a controlling terminal, so
  terminal prompts fail fast while askpass and agents keep working. Its
  process group is killed on timeout and after exit, and output is capped
  while it streams.
- **F5.** Inherited `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and related
  variables are removed, in the tool and in the fixtures.
- **F6.** The push refspec is pinned.
- **F7.** The change check runs before branching, and a failed commit
  returns to the original branch and deletes the new one.
- **Re-review of `d04deb5`, R1.** The preview is computed at the exact
  directory the tool runs in (`git_execution_root`, mirroring
  `ensure_worktree`): the session worktree, or a note that none exists yet.
  It is never computed in the main checkout. **L1.** URL credentials are
  stripped only from the authority. **L2, accepted residual risk.** A
  pinentry that gpg-agent starts through `GPG_TTY` runs outside Git's
  session and can still draw on the terminal. **L3, accepted.** Changes
  made between the preview and the commit are not shown, and the group kill
  after exit could in theory hit a reused group id.
- **F8, accepted.**
  - The protected-branch list (`main`, `master`, `origin/HEAD`'s branch) is
    fixed rather than configurable.
  - `nib run`'s console handler cannot push (fail closed until phase 6).
  - The approval pre-check does not model the terminal redirect.

Both tools:
- They are not offered over nib's MCP server.
- `git commit` and `git push` in `run_terminal` (including after `-C` or
  `-c` global options) are redirected to the tools before any approval or
  sandbox work.
- Approval-engine helpers (`automatic_decision`,
  `prompt_without_remembering`) keep `handle_approval` within the module
  size limit.

Plan mode uses the permission engine rather than the agent's planning mode,
which no interactive surface selects. This matches Claude Code's read-only
plan mode.

## User Decisions (2026-10-07)

- **D1: one spec.** All six phases are delivered under T080, with no split.
- **D2: in-place default, after checkpoints.** The user approved amending the
  governed instructions that mandate worktree-first editing:
  `workspace/instructions/tech/permissions.md` (principle 5, "Isolation
  First", and the worktree-default rules at lines 75 and 78) and
  `workspace/instructions/tech/backend_rust.md:24` ("Git worktrees isolate
  mutations"). The amendment lands in phase 5, only after phase 4 checkpoints
  ship. It is limited to: in-place editing as the default; worktrees as
  opt-in isolation for sessions and mandatory isolation for subagents and
  scheduled work; and the sandbox and permission-mode model of this spec.
- **D3: default mode `ask`.**
- **D4: network on in `workspace-write`.** Accepted risk: a command run after
  prompt injection could send workspace content over the network. Mitigations
  are the masked `$HOME` and `.nib` secrets, the filtered child environment,
  `ask` mode prompting for non-read-only commands, and `read-only` or
  per-project `network = "off"` configuration for sensitive repositories.
- **D5: multiple sessions per folder, as in Claude Code.** No folder lock.
  Conflicts are handled by a per-file stale-write check, and parallel work
  uses worktrees.
- **D6: branch, commit and worktree rules, as in Claude Code.** Use the
  current checkout and branch. Commit or push only on request. Branch first
  on a default or protected branch. Use worktrees only when isolation is
  needed. Explicit instructions win (section 7).

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-catalog-refresh | The shared release reservation was raised from patch to minor (0.3.1 → 0.4.0) on 2026-10-08 by user decision. T080 joins the T069 catalog-refresh release, development-start. The release owner applies the bump once; T080 does not apply a second bump. |

## Memory Impact

Status: pending
Rationale: Phase 1's sandbox decision (read-only Git metadata, trusted-layout
mounts) is recorded in decisions.md and changelog.md (2026-10-08). User decisions D1-D6 (in-place default after checkpoints, `ask` default, network on, a shared folder without a lock, the Claude Code branch and commit rules, and host-side commit/push) and the sandbox mount-plan contract are durable decisions. They are recorded in project memory when each phase is verified.

## Phase 1 Shared Integration (2026-10-08)

[PR52](https://github.com/skills-yaml/nib/pull/52) merged phase 1 into shared
`development` at `045e0e21d6e51488fcfcc544e2fceb35f0eb2d3d` on
2026-10-08T16:38:49Z. This confirms only the sandbox mount-plan phase.
T080 remains in development while its remaining phases and full acceptance
criteria are unresolved; this event does not establish main delivery or
publication. Synchronization combines that phase with T069-T073 under the
same applied minor 0.4.0 release and preserves the independent reviewed
sandbox implementation. Combined review and native gates precede the push.
