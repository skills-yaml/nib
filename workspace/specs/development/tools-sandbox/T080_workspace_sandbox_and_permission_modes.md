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

## Phase 3 Plan: Rules and "Always Allow" (2026-10-11)

Phase 3 delivers section 4 with these concrete choices.

**Rule syntax.** A rule is `tool` or `tool(specifier)`. The aliases
`terminal` and `bash` mean `run_terminal`, `edit` means `apply_patch`, and
`read` means `read_file`.
- A `run_terminal` specifier is an exact command (`cargo test`), or a word
  prefix ending in ` *` (`cargo test *`, which also matches `cargo test` on
  its own). The Claude Code spelling `cargo test:*` is accepted.
- A path specifier for `read_file`, `list_directory`, `grep` and
  `apply_patch` is a workspace-relative glob with `*`, `?` and `**`. For
  `apply_patch`, the paths come from the patch headers.

**Evaluation.** Deny beats ask, and ask beats allow. The phase 2
specific-over-general idea is dropped: a matching deny always wins, as in
Claude Code. Compound commands are judged part by part, splitting on `&&`,
`||`, `;`, `|` and newlines.
- An allow applies only if every part is allowed by a rule or is read-only.
- A deny or ask applies if any part matches.
- A command that cannot be split safely (quotes, substitution, redirection,
  a lone `&`) never matches an allow. It matches a deny or ask if a rule's
  command text appears anywhere in it.

**Layers.**
- **User:** the `[permissions]` table (`allow`, `ask`, `deny`) of
  `~/.nib/config.toml`. Only that table is read until T082 defines the rest
  of the file.
- **Project local (user-home):** `~/.nib/permissions.json`, keyed by the
  canonical project path. It holds the project's trust flag and the rules
  saved from prompts and `/permissions`. File edits cannot write it. On the
  Linux sandbox route tool commands cannot either, because the sandbox masks
  `$HOME`; on direct routes (macOS, Windows) an allowed command could.
- **Project shared:** the `nib-policy:` lines of `AGENTS.md`, `CLAUDE.md`
  and their `.local` variants, plus the legacy remembered-command file
  `.nib/command-prefixes.json`. Their deny and require-approval lines always
  apply. Their allow lines, and the legacy remembered commands, apply only
  after `/permissions trust`, because a cloned repository or an agent can
  write them.

`NIB_HOME` overrides `~/.nib` (used by tests).

**Prompt.** The remember choice becomes "Yes, and don't ask again for
`<rule>` in this project". It saves `run_terminal(<rule>)` to the project-local
layer. The suggested rule is the program and its subcommand word (plus a
third word after `run`, `exec` or `x`) followed by ` *`, for example
`cargo test *` or `npm run build *`. It falls back to the exact command when
the program wraps other commands (`sudo`, `env`, shells and interpreters,
`xargs`, `find`, `timeout`, `nice`, `nohup`, `exec`, `eval`, `command`,
`time`, `watch`, `ssh`), when the program is given by path, or when the
second word is not a plain subcommand word. Compound commands offer no
remember choice.

**`/permissions`.** Lists every rule with its layer, and marks ignored
project allow lines. Its subcommands: `allow|ask|deny <rule>` and
`remove <rule>` edit the project-local layer, and `trust`/`untrust` set the
project's trust flag.

**Hardening.** `apply_patch` refuses patches that touch `.nib` or `.git`
paths, so file edits cannot plant rules or Git configuration when phase 5
edits in place.

**Review of `c93ba01` (approve with fixes) and resolutions.**
- **F1, High.** `apply_patch`'s `.nib`/`.git` guard parsed patch headers
  itself, so C-quoted names (`"b/\056nib/x"`) and other prefixes slipped
  through. Paths now come from `git apply --numstat -z`, Git's own parser;
  a patch Git cannot read cannot be applied either.
- **F2, Medium.** `edit(<glob>)` matched `--- src/AGENTS.md`, which `-p1`
  writes as `AGENTS.md`. Same fix: rules see Git's paths.
- **F3, Medium.** An exact suggestion such as `sudo rm *` was re-parsed as a
  prefix rule. Commands with glob characters (`*`, `?`, `[`) offer no
  remember choice.
- **F4, Medium.** Deny and ask rules for commands now look through
  `NAME=value` words and transparent wrappers (`sudo`, `env`, `timeout`,
  `xargs` and others, trying every word after the wrapper), program paths and
  case. Unsplittable commands are compared word by word after removing
  quotes. Documented as a guardrail, not a sandbox.
- **F5, Medium.** `read(...)` covers `read_file`, `list_directory` and
  `grep`. Paths are matched lexically and after resolving symbolic links,
  relative to the session directory or the project root, ignoring ASCII case.
  A deny or ask on an anchored folder also covers a `grep` of any folder above
  it. Absolute and `..` rule paths are rejected.
- **F6, Medium (design).** Edits to top-level `AGENTS.md`, `AGENTS.local.md`,
  `CLAUDE.md` and `CLAUDE.local.md` always ask, even in `accept-edits` and
  `auto` or with allow rules, and are refused when nobody can answer
  (`always_asks`, shared with `git_push`). A terminal command allowed by a
  rule could still write them; accepted residual risk.
- **F7, Low-Medium.** The wrapper list now includes process wrappers,
  interpreters, package runners, `git`, `docker`, `podman` and `kubectl`, and
  the word after `run`/`exec`/`x`/`dlx` is checked against it.
- **F8, Low.** A linked worktree uses its main project's rules and trust,
  accepted only when Git's back-link (`<gitdir>/gitdir`) points at this
  worktree, so a copied `.git` file cannot claim another project's trust.
- **F9, Low.** Spec and guide wording now limit the protection of `~/.nib` to
  file edits everywhere and to commands on the Linux sandbox route.
- **F10, accepted.** A command rule ignores `cwd`, timeout and background,
  as in Claude Code. Documented.
- **F11, Low.** When a rules file exists but cannot be read, every action asks
  (headless runs deny) until it is fixed, and `/permissions` says so.
- **F12, Low.** Saving forgets projects whose folder no longer exists and caps
  the file at 1000 projects.
- **F13, Low.** Unit tests use a thread-local nib home (`TestNibHome`) and
  never read the developer's home or the environment. `task test` and
  `task test:integration` set `NIB_HOME` under `target/`, and the
  integration fixtures that need trust write it there.
- **F14, Low.** Rules are stored in canonical spelling, so `bash(x:*)` and
  `terminal(x *)` are one rule and either removes it. `/permissions` counts
  the instruction-file allow lines that trust would enable.

**Re-review of `ffa9163` (approve with fixes) and resolutions.**
- **N1, Medium.** Inside a repository subdirectory Git reports patch paths
  from the repository top, so a nested project's `AGENTS.md` read as
  `proj/AGENTS.md` and skipped the always-ask check. `git_patch_paths` strips
  `git rev-parse --show-prefix`; regression tests cover rules and
  `always_asks`.
- **N2 and N3.** A `grep` of any folder reaches a file-name glob (`*.pem`)
  unless it names hidden files, which the search skips, and reaches any glob
  once a `**` part is reached.
- **N4.** Deny and ask rules now match at any word of each command part
  (`busybox rm`, `sh -c rm`), and a command word with glob characters is
  matched as a pattern (`/bin/r?`). Over-matching is accepted; variable
  composition (`x=r; ${x}m`) is out of reach and documented.
- **N5.** In a linked worktree, allow lines come only from the main
  checkout's instruction files (when trusted), never from the worktree's own
  files, which sandboxed commands can write.
- **N6.** Pruning keeps entries that hold deny or ask rules.
- **N7.** A deny or ask entry that cannot be parsed makes every action ask.
- **N8, partly accepted.** A call's paths are computed once per rule
  evaluation. The remaining few synchronous `git apply --numstat` runs per
  patch approval are short and accepted.
- **N9.** Versioned interpreters (`python3.12`) and runner subcommands that
  name a wrapper (`yarn node`) suggest only the exact command; a trailing dot
  on `AGENTS.md.` still counts as an instruction file. Running `cargo test`
  without the Taskfile can still read the developer's `~/.nib` in
  integration tests that do not set `NIB_HOME`; accepted.

**Final re-review of `335e1a3`: approve.** Its three low notes are fixed
in the delivery candidate: the legacy remembered-command file is ignored in
linked worktrees, deny matching understands `[...]` classes in command
words, and `git rev-parse --show-prefix` scrubs the same Git variables as the
numstat call.

**Out of scope.** Project `.nib/config.toml` settings other than rules
(for example `approvals.mode`) still apply without trust. That is recorded
as a T082 follow-up. The stale-write check stays in phase 5.

## Phase 4 Plan: Checkpoints and `/rewind` (2026-10-12)

Phase 4 delivers section 5 with these concrete choices.

**One checkpoint per turn.** A turn starts with each human request,
steering message or question answer in the session. The first file edit
of a turn opens that turn's checkpoint, labelled with the request text.
Turns that neither edit files nor run a command that may write them have no
checkpoint.

**What is captured.** Before `apply_patch` writes, nib saves the prior
state of every path the patch touches, taken from Git's patch parsing
(phase 3). It saves each path once per turn:
- a file's bytes and executable bit;
- a symlink's target;
- or "absent" when the patch creates the file.

Contents go to a content-addressed blob store under
`.nib/profiles/<id>/checkpoints/<session>/`. On the Linux sandbox route
that directory is masked, so tool commands cannot read or forge it. On
direct routes (macOS, Windows, `full-access`), or with a profile
`state_dir` outside `.nib`, an allowed command could change it. Forging
gains little: a restore still writes only inside the session's workspace,
never `.git` or `.nib`.

**Terminal commands.** Commands are not snapshotted in this phase. When a
turn ran a command that is not read-only, a subagent merge or an MCP tool,
its checkpoint records that.
`/rewind` then says files changed by commands are not restored. Restoring
command effects needs the terminal-observed diff that phase 5 builds for
in-place reconciliation (section 6b). Copying the whole tree each turn, or
running the host's Git on an untrusted repository, would be too costly or
unsafe here.

**`/rewind`.**
- `/rewind` lists the checkpoints, newest first. Each shows its number,
  time, request text, file count and the command warning.
- `/rewind <n>` restores every file edited since checkpoint `n` began to
  its state before that turn. Files the agent created are removed, and
  files it deleted come back.
- It first saves the current state of those files as a new checkpoint
  ("before rewind to #n"), so a rewind can itself be undone.
- It writes only inside the recorded workspace root, which must be the
  project or the session's validated worktree. Files whose workspace is
  gone (a landed or removed worktree) are reported, not written. It never
  writes `.git` or `.nib`, and refuses paths that escape the root or pass
  through a symlink.
- Each directory is opened through a handle that does not follow links.
  Files are read the same way, and written to a new file that is renamed
  over the target. A concurrent swap to a link therefore cannot redirect
  a read or a write. Files that already match are left alone.
- A rewind overwrites concurrent edits to the same files, whether by
  another session or by the user. Its undo checkpoint keeps them.
- The conversation is not rewound. nib appends a runtime note listing the
  restored files so the model re-reads them.
- `/rewind` needs an idle session.

**Bounds.**
- A file over 16 MiB is recorded as unavailable and reported by `/rewind`.
- A session keeps at most 100 checkpoints and 256 MiB of blobs. The oldest
  checkpoints and unreferenced blobs are pruned first.
- A storage failure never blocks the edit. It is logged, and `/rewind`
  reports the missing checkpoint.

**Out of scope.** Rewinding the conversation, restoring command effects
(phase 5), subagent worktrees (their merge is the review point) and moving
the store to user-home state (T082).

**Independent review of `a2773f4`: changes requested, then resolved.**
- **F1 (medium-high).** The undo checkpoint's pruning could drop the
  checkpoint being restored, together with its blobs. Fixed: a rewind
  saves without pruning, and prunes only after the restore. The size cap
  drops whole checkpoint numbers and always keeps the newest.
- **F2 (medium).** A check-then-use race could redirect a restore into
  `.git`, or a capture into `.nib`. Fixed with the handle-based I/O
  described above.
- **F3.** Overstated store protection. Fixed by naming the unsandboxed
  routes.
- **F4.** Subagent merges and MCP tools are now flagged.
- **F5.** An index with no new paths is not re-saved. Not done: reusing
  the patch paths computed during approval, and moving the I/O to a
  blocking thread. Edits are small and the extra Git calls are bounded;
  this is accepted as a later optimisation.
- **F6.** Unchanged files are left alone.
- **F7.** Lossy non-UTF-8 names are recorded as unavailable.
- **F8.** A gone workspace is skipped and reported, instead of failing the
  whole rewind.
- **F9.** Wording fixed.

**Re-review of `be89ed0`: approve.** Its four low notes are fixed in the
delivery candidate:
- a later rewind removes a `.nib-rewind-*` temporary file a crash left in
  a directory it restores into;
- on Windows, a directory link or junction is removed with `remove_dir`;
- files keep their saved Unix permission bits (`0o777`, never setuid);
- the link tests use links to `.git` and `.nib` inside the workspace.

## Phase 5 Delivery Split (2026-10-13)

Phase 5 is split like phase 2, so each part stays reviewable.

- **5a: shared-folder safety.** The stale-write check (AC-6d) and session
  presence (AC-6e). Both protect sessions that already share a folder
  today, for example the user editing while nib works, and are needed
  before the in-place default.
- **5b: in-place reconciliation records.** Path-scoped in-place
  verification with the terminal-observed diff and the per-run records
  (AC-6b). It applies wherever nib already works in place (a project that
  is a linked worktree), so it can land and be tested before the default
  changes.
- **5c: in-place by default.** The `execution.workspace` setting,
  `--worktree` and `/worktree`, mandatory worktrees for subagents,
  scheduled work and MCP-server calls, instruction-required isolation, the
  sandbox and subagent-merge changes the default needs, and the D2
  governed-instruction amendments (AC-3 in part, AC-6h, AC-8).
- **5d: `/land`.** Preview, then commit to the session branch, merge into
  the user's branch, or discard, each after a prompt (the rest of AC-3).

Worktrees stay the default until 5c lands. (Re-split on 2026-10-13 after
mapping the code: 5b as first planned touched the executor, agent loop,
sandbox, delegation, daemon and interactive layers at once.)

### Phase 5a Plan

**Stale-write check.**
- After a successful `read_file`, nib records a fingerprint of the whole
  file for the session: the SHA-256 of its bytes, or of a symlink's target.
  Files over 16 MiB are fingerprinted by size and modification time (plus
  device, inode and change time on Unix) instead, which bounds the work per
  read. The fingerprint is
  taken before and after the read and recorded only if both match, so the
  file did not change during the read. A partial read (a line range, or a
  file beyond the read limits) still counts as a read, as in Claude Code. A
  read through a symlink also records the link itself, because a patch
  addresses the link.
- Before `apply_patch` runs, dry run or not, every path Git reports for the
  patch is checked:
  - an existing file the session never read fails with
    `read <path> with read_file before editing it; ...`;
  - a file whose fingerprint changed since the read fails with
    `file changed since you read it; re-read <path> and retry`;
  - a path that does not exist is a creation and needs no read, even when
    the session read it before it was deleted (nothing can be overwritten,
    and a re-read of a missing file could never succeed);
  - a path that cannot be keyed (a name Git reports lossily, or one outside
    the workspace) fails with `cannot check <path> ...`.
  Nothing is written when the check fails.
- After a real patch succeeds, nib records the new fingerprints, so
  consecutive edits need no re-read. Deleted files are forgotten.
- Records are keyed by the on-disk path below the workspace top. A read in
  the project and an edit in the session worktree therefore agree when the
  contents are the same, and must re-read when they differ. Existing patch
  targets are keyed by their canonical name, as reads are, so a
  case-insensitive file system cannot make the two disagree.
- Records live in `.nib/profiles/<id>/file_reads/<session>.json`, one file
  per session, written atomically. A session keeps at most 10,000 files and
  forgets the least recently read first. A damaged record is reset, which
  only costs re-reads.
- The check is skipped for calls without a session, which cannot edit a
  shared folder. Terminal commands are not covered (section 6).
- Accepted limits:
  - The check and the write are separate steps. A write landing between
    them is not detected. The window is the duration of one `git apply`.
  - Likewise, a write landing between `git apply` and the new fingerprint
    is recorded as the session's own, so its next patch is not flagged.
  - Fingerprints are computed on the executor's thread. Hashing is bounded
    by the 16 MiB limit (twice per read); moving it to a blocking thread is
    a later optimisation, like phase 4's F5.
  - A patch that fails the check after the user approved it must be
    approved again once re-read.
  - On the direct routes, a tool command could rewrite the record. It
    would only weaken this session's own check.
- Fingerprints never follow a final symlink, and never open FIFOs or
  devices, so a check cannot block.

**Session presence.**
- Each interactive session writes a heartbeat file to
  `.nib/profiles/<id>/presence/<session>.json` every 30 seconds, naming its
  canonical folder. The TUI beats from its draw loop; plain mode and
  `nib run` keep a heartbeat from a background thread, which follows a
  plain-mode session switch.
- The TUI footer re-counts the other sessions in the same folder every 5
  seconds and shows `1 other session in this folder` (or `N other
  sessions`). It is informational only and blocks nothing.
- A heartbeat older than 2 minutes is ignored, so a crashed session
  disappears. Files older than a day are removed by any scan. A session
  removes its own file when it exits or switches session.
- Subagents and scheduled work run in worktrees and are not counted.
- Presence is per profile until T082 moves state to user home.

**Out of scope for 5a:** everything listed under 5b. Until 5b, sessions
edit in their own worktree, so the check mainly protects in-place runs (a
project that is itself a linked worktree) and reads in the checkout that
differ in the session worktree.

**Independent review of `2dbef20`: changes requested, then resolved.**
- **F1 (high).** A file the session had recorded and that was then deleted
  (by `/rewind`, a command or another session) could never be created
  again, since a re-read of a missing file fails. Fixed: a missing target
  is always a creation.
- **F2 (medium).** Whole-file hashing of huge files blocked the executor.
  Fixed with the 16 MiB metadata fingerprint; moving hashing off the async
  thread is accepted as a later optimisation.
- **F3 (medium).** Plain interactive mode wrote no heartbeat. Fixed.
- **F4.** Lossily decoded or unkeyable patch paths skipped the check. They
  now fail closed.
- **F5.** The apply-to-record window is listed as an accepted limit.
- **F6.** Patch targets are keyed by their canonical on-disk name.
- **F7.** The guide and this plan describe today's worktree default and
  partial reads accurately.
- **F8.** Tests added: the footer with other sessions, a read through a
  link, a refused edit leaves no checkpoint, re-creating a deleted file,
  unkeyable paths and the large-file fingerprint.

**Re-review of `9c37455`: approve.** Its two low notes need no code change:
the large-file fingerprint's Unix-only fields are now stated above, and a
patch through a symlinked directory is refused by `git apply` either way.

### Phase 5b Plan

Goal: in place, a session's verification depends only on the files that
session changed, so another session's edits elsewhere in the folder do not
make it stale (section 6b, AC-6b).

**Where a call runs.** A call runs *in place* when its execution root is
not a nib-managed worktree (`.nib/worktrees/<kind>/<name>`, also when the
project itself is a linked worktree). Today that is a project that is itself
a linked worktree; after 5c it is the default.
Changed paths are keyed, as in 5a, by the path below the project top.

**Changed paths per call.** Each audited tool record gains an optional
`changed_paths` field, `{root, paths, complete}` (`root` is the project
top), recorded only in place:
- `apply_patch` (not a dry run, succeeded): the patch's target keys,
  including both names of a rename. Complete.
- A foreground `run_terminal`, `git_commit` or `git_push` that is not
  read-only: the observed diff (below). Commits and pushes are observed
  because the user's hooks may rewrite files. Complete unless the
  observation was cut short.
- A dry run, a patch that did not apply (`git apply` is all or nothing), a
  read-only command or built-in tool, or a call refused before it ran: an
  empty, complete record ("changed nothing").
- Any other call (a background command, a subagent merge, any MCP tool,
  an unknown tool): incomplete, because nib cannot see what it changed. MCP
  tools are always classified as network risk, so even read-only ones
  disable narrowing for the session.
- Built-in tools that never edit files (`read_file`, `grep`, plan, task,
  memory and subagent management) and the profile curator's record record
  nothing.
- Calls in a managed worktree record nothing; the worktree is their
  isolation, and verification keeps covering the whole worktree.

**Terminal-observed diff.** Before and after an in-place command, nib takes
a metadata snapshot of the project: for every entry, its type, size and
modification time (plus inode and change time on Unix), and a symlink's
target. The paths that appear, disappear or differ are the command's
changed paths.
- The walk skips `.git`, `.nib`, `target` and `node_modules`, never
  follows symlinks, and runs on a blocking thread.
- It stops at 50,000 entries. A snapshot that was cut short, or that
  failed, makes the call's record incomplete.
- No Git command runs, so it works without bwrap and on every platform.

**Session changes.** The session's changed paths are the union of its
in-place records. The set is unknown when:
- any record is incomplete, or names another project top;
- a record of a tool that may edit files has no `changed_paths` (written
  before 5b, or in a managed worktree);
- a `tool_attempted` event has no matching record (for example a run
  cancelled mid-command);
- it grows past 1,000 paths.

**Path-scoped identity.** A verification obligation that covers the whole
folder (`affected_paths` is `["."]` or empty, for example a required
command such as `task verify`) is narrowed when it passes in place:
- it records `identity_scope {root, paths}`, the session's changed paths
  at that moment, and computes its content identity over exactly those
  paths, relative to the project top;
- revalidation recomputes the identity over the same scope, so a later
  change to one of them, by anyone, makes it stale;
- revalidation also makes it stale once the session's own changes are
  unknown or include a path outside the scope, even when the call that
  changed it failed and so never invalidated the check (review F1);
- an edit by another session to any other file does not;
- if the session changes are unknown, nothing is narrowed: the identity
  covers the whole folder, as today. The project top is taken from the
  session's in-place records (the verification command's own first), so a
  session with no in-place record at all also keeps the whole folder.
Obligations with planner-declared paths keep those paths. The session's own
successful mutations still invalidate its passed checks as today; section
6b only narrows what *other* writers can invalidate.

**Run records.** In the session's event log:
- `run_started` adds `workspace` (`in_place` or `worktree`), the
  workspace root, the latest checkpoint number before the run (or none),
  and `other_sessions`, the ids of the other live sessions in the folder
  from 5a presence.
- `run_terminal` adds `changed_paths` (the paths this run changed, or
  `unknown`) and, in place, the content identity over them.

**Accepted limits.**
- A file another session writes while this session's command runs is
  attributed to the command. That only widens this session's identity
  (more invalidation, never less).
- A same-size rewrite within one modification-time tick is not observed.
  Nanosecond times and the change time make this rare.
- Writes into skipped directories (`target`, `node_modules`, at any
  depth) are not observed; they are build output. A source directory with
  one of those names is not observed either.
- Writes made after the second snapshot by a process the command left
  running are not observed.
- Non-UTF-8 names, unreadable directories or more than 50,000 entries make
  every command's record incomplete, which keeps the whole-folder identity.
- Each in-place command that is not read-only costs two metadata walks.
- The run-end identity is hashed when the run ends (at most 1,000 paths and
  64 MiB). A run that is cancelled writes no `run_terminal` event, as
  before.
- `workspace_root` is stored unredacted. If redaction changes the project
  top, tool records and run records disagree and `run_terminal` reports
  `unknown`.
- A passing check in place may depend on another session's unverified
  edits to other files (section 6b). The run record lists the other
  sessions that were active.

**Tests.**
- Snapshot diffs: creation, deletion, a rewrite, a symlink retarget,
  skipped directories, and the entry cap making a record incomplete.
- Executor: an in-place patch and an in-place command record their changed
  paths; a managed worktree records none; a dry run and a refused patch
  record "changed nothing".
- Verification: in a shared linked-worktree folder, session A runs a
  required command after editing `a.txt`. Session B's edit to `b.txt`
  leaves A's check passed. B's edit to `a.txt` makes it stale. An unknown
  change set, a foreign top and a record without change details keep the
  whole-folder identity. A's own later change outside its scope (from a
  failed call) and an attempt without a record make the check stale.
- Managed worktrees are recognised inside a linked-worktree project.
- Run records carry the workspace, the checkpoint, the other sessions and
  the changed paths.

**Independent review of `6955e4f`: changes requested, then resolved.**
- **F1 (high).** A narrowed check ignored the session's own later change
  outside its scope when the call that made it failed (and so never
  invalidated). Fixed: revalidation stales a check whose session changes
  are unknown or outgrow its scope.
- **F2 (medium).** A missing record counted as "changed nothing" (legacy
  records, cancellation). Fixed: dry runs, read-only calls and refusals now
  record an empty change set; a mutating record without one, or an attempt
  without a record, makes the set unknown.
- **F3 (medium).** `git_commit` and `git_push` run the user's hooks and
  are now observed like commands.
- **F4 (medium).** Subagent worktrees of a linked-worktree project were not
  recognised as managed. Fixed and tested.
- **F5 (low).** Read-only built-in calls record "changed nothing". MCP
  tools are always network risk and still disable narrowing (safe; stated
  above).
- **F6-F8 (low).** Limits, field names and the catalog row are now stated
  accurately.
- **F9 (low).** Tests added for F1, F2 and F4. Rename keys are covered by
  5a's `tracked_patch_paths`; `git_commit` hooks and cancellation timing
  are covered by the session-level rules rather than end-to-end fixtures.

**Re-review of `625eefc`: F1-F4 confirmed fixed; three new findings, resolved.**
- A real patch that failed to apply was recorded without change details,
  making the session's changes unknown and staling passed checks. It now
  records "changed nothing".
- The daily profile-curator record (profile state only) is exempt.
- The F5 note above is corrected: MCP tools still disable narrowing.
The accepted limits for F7 were agreed.

**Re-review of `54104c6`: approve**, with no new findings.

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

## Integration Evidence (per phase)

- **Phase 1** (sandbox mount plan): PR 52 merged the verified candidate
  `ea2ac61` into `development` as
  `045e0e21d6e51488fcfcc544e2fceb35f0eb2d3d` on 2026-10-08T16:38:49Z.
- **Phase 2a** (permission modes): PR 54 merged the verified candidate
  `0e8ecaf` as `21ccf1dcd4599900a242e889554f0fcd52ca4660` on
  2026-10-09T20:10:07Z.
- **Phase 2b** (host-side `git_commit` and `git_push`): PR 55 merged the
  verified candidate `11efc46` as `6e14f30e013b3699760d54563f542035e93ab32a`
  on 2026-10-10T21:15:43Z.
- **Phase 3** (permission rules, project trust, always-allow): PR 56 merged
  the verified candidate `b217eac` as
  `8c0eabb4d1ea768dee9b5d20872aa49a8181bc44` on 2026-10-12T21:14:08Z. The
  first macOS CI run timed out in the unrelated process-timing test
  `successful_sync_leader_does_not_wait_for_descendant_owned_pipes`; the
  unchanged re-run passed.

For each phase, before the merge:
- independent exact-candidate review approved it;
- a serial `task verify` passed with `NIB_REQUIRE_BWRAP_TESTS=1`;
- PR CI passed Linux Validate and macOS; the workflow skipped Windows.

After each merge, `development` was re-read and contained the candidate
with no file difference. T080 stays in development until every phase is
integrated.
