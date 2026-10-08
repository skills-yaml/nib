# T082: User-Home Runtime State and SQLite Sessions

**Status:** Backlog. Open decisions are listed below and must be resolved
before development.
State: backlog
Primary Feature: architecture

## Problem and Authority

Requested by the user on 2026-10-07: "move .nib to user home under .nib, move
nib sessions to sqlite under .nib in the user [home]; for the moment .nib under
project can only store user specific preferences on the project like skills
and models preferences."

Today, all nib state lives in `<project>/.nib/`. The 2026-10-07 inventory
found these problems:

- **Credentials in the project.** `nib auth` writes plaintext
  `llm.providers.*.api_key` into `<project>/.nib/config.toml`
  (`src/auth.rs:4,125`). This file sits next to the code, is copied with the
  repository, is readable by supervised sandboxed commands (see
  [T080](../../development/tools-sandbox/T080_workspace_sandbox_and_permission_modes.md)), and has
  no `.gitignore` protection.
- **No user-global layer.** Config is project-only
  (`load_nib_config_full`, `src/config/split_00.rs:1553`), and every project
  needs its own credentials.
- **Copy fragility.** Runtime state is bound to inode identities. Copying or
  restoring the project on 2026-10-03 invalidated 23 worktree ownership
  receipts, and `nib doctor` failed until the receipts were re-recorded by
  hand on 2026-10-06.
- **Flat JSON sessions.** 163 session files are stored as whole-file JSON
  rewrites. Concurrency relies on striped lock files, a run lease, a directory
  identity file and read-back verification (`src/session/split_02.rs`).
- **No central `.nib` path.** Paths are built by hand at about 45 sites, and
  some code assumes `.nib` is inside the project (`verify.rs:621`,
  `delegation/part_06.rs:1648`, `daemons/workload/split_00.rs:365`,
  `delegation/part_01.rs:448`, `daemons/state/split_01.rs:1182`).

## Scope and Proposed Behavior

### Target layout

```text
~/.nib/                              (NIB_HOME overrides; 0700)
  config.toml                        user config: credentials, provider endpoints,
                                     default provider/model, approvals; 0600
  nib.db                             SQLite (WAL): sessions, events, messages,
                                     tool calls, plans, schema_version
  projects/<project-key>/            per-project runtime state
    project.toml                     canonical root and git common dir (identity)
    memory.json, daemons/, plans/, subagents/, process-scopes/,
    worktree-ownership/, command-prefixes.json, trust (workspace.allowed)
    worktrees/{sessions,subagents}/  managed git worktrees
<project>/.nib/                      preferences only, no secrets, no runtime state
  config.toml                        model/provider selection among user-configured
                                     providers, skill selection, agent preferences
  skills/<name>/SKILL.md             project skills
```

`<project-key>` is a stable hash of the canonical project root. `project.toml`
records the root and the git common directory, so a moved or copied project can
be detected and re-linked explicitly through `nib doctor --fix`. Inodes are no
longer the only identity.

### Rules

1. **One path authority.** A new `NibPaths` module resolves `user_home`,
   `project_state(project)` and `project_prefs(project)`. All hand-built
   `.join(".nib")` sites are migrated to it, and a lint/test forbids new ones
   outside the module.
2. **Config layering.** Precedence is: environment, then `~/.nib/config.toml`,
   then project preferences. Project preferences cannot set `api_key(s)`,
   `base_url`, endpoint, transport, approvals, sandbox or trust. A cloned
   repository must not be able to redirect user credentials to its own
   endpoint or relax safety. Such keys in project config are rejected with a
   diagnostic. `nib auth` writes only to the user config.
3. **SQLite sessions.** `SessionStore` keeps its public API (`load_result`,
   `save`, `update_session`, `record_event`, `append_message`, `list`, ...).
   Its backend becomes `nib.db`:
   - Sessions start as one row per session with a JSON document column and a
     `revision` column. Optimistic concurrency uses
     `UPDATE ... WHERE revision = ?`.
   - Events and messages are append-only child tables, indexed by session and
     project.
   - Transactions replace the striped lock files and read-back verification.
     The run lease becomes a row with a holder PID and heartbeat.
   - `PRAGMA journal_mode=WAL`, a `busy_timeout`, and migrations versioned in
     `schema_version`.
4. **Sessions are scoped by project.** Each row carries `project_key` and
   `profile_id`, and `/sessions` lists the current project unless asked for all
   projects.
5. **Worktrees outside the project.** Under [T080](../../development/tools-sandbox/T080_workspace_sandbox_and_permission_modes.md),
   sessions work in place by default and worktrees are opt-in. Session
   checkpoints and write leases also live in user-home state. Opt-in session
   worktrees and subagent worktrees are
   created under `~/.nib/projects/<key>/worktrees/`. Path-prefix checks
   (`ownership.rs:460`, `delegation/part_07.rs:769`, `part_06.rs:1424`) are
   rebased onto `NibPaths`. Lock anchors no longer depend on a directory named
   `.nib` inside the repository (`daemons/state/split_01.rs:1182`).
6. **Migration.** On first run of the new version, a one-time, resumable
   migration runs, recorded in `nib.db` and confirmed in the TUI. It imports
   `<project>/.nib/profiles/*/sessions/*.json` into `nib.db`, moves memory,
   daemons, subagents, plans and receipts into `projects/<key>/`, moves
   credentials and endpoints to the user config, and rewrites the project
   config to preferences only. Originals are kept in
   `<project>/.nib/legacy-<timestamp>/` until the user removes them. The
   migration is idempotent and safe to interrupt.

## Open Decisions (user)

1. **Existing session worktrees.** The options are (a) leave the 30 existing
   worktrees under `<project>/.nib/worktrees` as legacy and still manage them,
   or (b) migrate them with `git worktree move` and fresh receipts.
   Recommended: (a). New worktrees are created in the user home, and legacy
   worktrees remain manageable until they are cleaned.
2. **Shape of the session table.** The options are a JSON document per session
   first, with normalization later, or a fully normalized table set now.
   Recommended: document plus append-only events and messages, which keeps
   `SessionStore` semantics and limits risk.
3. **Approvals and trust as preferences.** Recommended: keep them user-level
   per project (`projects/<key>/`), not in project `.nib`, because a
   repository could grant itself approvals.
4. **SQLite dependency.** `rusqlite` with the `bundled` feature adds C code to
   the build on Linux, macOS and Windows. AGENTS.md requires an explicit
   decision and a tech-doc update
   (`workspace/instructions/tech/backend_rust.md` is governed and needs scoped
   approval).
5. **Location on Windows and macOS.** The options are `~/.nib` everywhere,
   which matches the request, or platform data directories. Recommended:
   `~/.nib` everywhere, with a `NIB_HOME` override.

## Exclusions and Compatibility

- No change to session semantics, plan or workload models, or tool contracts.
- Project skills stay in `<project>/.nib/skills`, and global skill lookup
  (`~/.config/nib/skills`, `NIB_SKILLS_DIR`) is unchanged.
- Subagent worktrees no longer force a `state_dir` inside the worktree
  (`delegation/part_06.rs:952`). They use the parent project's user-home state.
- This spec does not add cross-machine sync, encryption at rest or a keyring.
  It enforces 0600 file permissions, and keyring integration is a later
  follow-up.

## Affected Areas

`src/config/` (layering, `nib auth`), `src/profile/` (paths, migration),
`src/session/` (store backend), `src/daemons/` (state, workload, curator,
cron, task), `src/tools/delegation/`, `src/tools/core.rs`,
`src/sandbox/{worktree,process}/`, `src/integrations/worktree.rs`,
`src/interaction_card.rs`, `src/context/{skills,agents}.rs`, `src/doctor.rs`,
`src/agent/loop/verify.rs`, `Cargo.toml` (rusqlite), user guide, tech docs
(with approval), catalog, releases and memory.

## Phased Delivery

1. Add `NibPaths` and migrate all path sites with no layout change (pure
   refactor; the existing tests guard it).
2. Add user config layering and project-preference restrictions, and move
   `nib auth` to the user config.
3. Move runtime state to `~/.nib/projects/<key>/`, including new worktrees,
   with the migration for non-session state.
4. Add the SQLite session backend behind `SessionStore` and import the JSON
   sessions.
5. Update doctor (new layout, project re-linking after copy or move, legacy
   report) and add the documentation.

Each phase is independently reviewable and keeps `task verify` green.

## Acceptance Criteria

- [ ] AC-1: A fresh project using the new version creates no runtime state or
  secrets under `<project>/.nib`. Only preference files may appear there.
- [ ] AC-2: `nib auth` stores credentials only in `~/.nib/config.toml` with
  mode 0600. Project config that sets credentials, endpoints, approvals,
  sandbox or trust is rejected with a diagnostic.
- [ ] AC-3: Sessions are persisted in `~/.nib/nib.db`. Concurrent writers are
  rejected on revision conflict, and run leases are exclusive across
  processes. Existing session tests pass against the SQLite backend.
- [ ] AC-4: The migration imports every existing JSON session (count and
  content parity checked), moves runtime state and credentials, keeps the
  originals in the legacy directory, and is idempotent and resumable after
  interruption.
- [ ] AC-5: Copying or moving the project directory does not break nib.
  `nib doctor` detects the mismatch and `--fix` re-links with confirmation.
- [ ] AC-6: No `.join(".nib")` remains outside `NibPaths`, and a test enforces
  this.
- [ ] AC-7: Linux, macOS and Windows CI pass with the bundled SQLite. Doctor
  reports the new layout. Guide, tech docs, catalog, versions and memory are
  reconciled, and an independent review is complete (data integrity and
  credential handling).

## Validation Gates

`task check` and focused session/config/delegation/worktree tests for each
phase, then `task verify`, `task docs:check`, `task versions:check` and the
native all-platform CI. The migration is tested against a fixture copy of a
real legacy `.nib` tree. An independent exact-candidate review is mandatory
because this touches data integrity, credentials and a public layout.

## Risks and Rollback

- **Data loss during migration.** Mitigated by copy-then-verify, keeping the
  originals and making the migration resumable.
- **Mixed binary versions.** An older nib still reads `<project>/.nib`. The
  migration writes a marker that older binaries treat as "state moved" to
  avoid split-brain, and this must be tested.
- **Native build impact of bundled SQLite.**
- **Rollback.** Revert the binary and restore from the legacy directory.
  `nib.db` is kept. Published versions are never reused.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | none | none | Backlog proposal; expected minor reserved at development start. Changes the persisted state layout and storage backend, with automatic migration; user-visible config location change. |

## Memory Impact

Status: none
Rationale: Backlog proposal; the durable decision is classified as pending at development start and recorded when implemented.
