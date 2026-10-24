# T082: User-Home Runtime State and SQLite Sessions

**Status:** Development. Started on 2026-10-22 after the user accepted every
recommended decision below.
State: development
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

### Decisions (2026-10-22)

The user accepted every recommendation: (1) existing worktrees stay where they
are as legacy and remain manageable, while new ones go to the user home; (2) a
JSON document per session plus append-only events and messages; (3) approvals
and trust are user-level per project; (4) `rusqlite` with the `bundled`
feature, and the user approved the scoped amendment to the governed
`workspace/instructions/tech/backend_rust.md` that records this dependency;
(5) `~/.nib` on every platform, with a `NIB_HOME` override.

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

## Phased Delivery (Implementation Plan)

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
- [x] AC-2: `nib auth` stores credentials only in `~/.nib/config.toml` with
  mode 0600. Project config that sets credentials, endpoints, approvals,
  sandbox or trust is rejected with a diagnostic. Evidence (phase 2):
  `auth::tests::auth_wizard_accepts_numeric_provider_and_one_shot_mode`
  (user config only, mode `0600`, no secret in the project),
  `config::layers::tests::a_shipped_project_file_cannot_redirect_credentials_or_relax_safety`,
  `project_preference_edits_cannot_set_user_only_values` and
  `user_values_added_after_the_migration_are_rejected`. Existing untracked
  legacy files are migrated instead of rejected (see phase 2 notes).
- [ ] AC-3: Sessions are persisted in `~/.nib/nib.db`. Concurrent writers are
  rejected on revision conflict, and run leases are exclusive across
  processes. Existing session tests pass against the SQLite backend.
- [ ] AC-4: The migration imports every existing JSON session (count and
  content parity checked), moves runtime state and credentials, keeps the
  originals in the legacy directory, and is idempotent and resumable after
  interruption.
- [ ] AC-5: Copying or moving the project directory does not break nib.
  `nib doctor` detects the mismatch and `--fix` re-links with confirmation.
- [x] AC-6: No `.join(".nib")` remains outside `NibPaths`, and a test enforces
  this. Evidence (phase 1): every production path site uses
  `crate::paths::NibPaths` (`src/paths.rs`), and
  `paths::tests::no_hand_built_nib_paths_outside_this_module` fails on any
  `".nib"` or `".nib/…"` literal in production code outside that module. Test
  fixtures that build a legacy project tree on purpose keep their literal
  paths; the scan skips test-only files and inline test modules.
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

## Implementation Notes

### Phase 1: one path authority (2026-10-22)

- `src/paths.rs` adds `NibPaths` with `user_home`, `project_state`,
  `project_prefs` and the derived locations (`project_skills`,
  `project_plans`, `managed_worktrees`, `session_worktrees`,
  `subagent_worktrees`, `relative_profile_state`,
  `relative_project_skills`), plus `PROJECT_DIR_NAME`,
  `PROJECT_DIR_EXCLUDE_PATHSPEC` and the directory-name checks used by the
  patch, commit, checkpoint, sandbox-mask and lock-anchor guards.
- `user_home` moved from `tools::permission_rules::nib_home` unchanged
  (`$NIB_HOME`, else `~/.nib`; unit tests see only `TestNibHome`), and
  `TestNibHome` moved with it.
- All production `.join(".nib")` sites (config, profiles, sessions, memory,
  daemons, curator, delegation, process scopes, worktree ownership and
  cleanup, sandbox mounts, interaction card, doctor, MCP test fixture) now
  call `NibPaths`. `project_state` and `project_prefs` still both return
  `<project>/.nib`, so the layout is unchanged.
- Validation: `task check` passed; focused lib tests for `paths`, `config`,
  `profile`, `session`, `sandbox`, `tools::delegation`,
  `tools::permission_rules`, `interaction_card`, `daemons`, `context::skill`,
  `integrations::worktree`, `tools::git*`, `tools::checkpoints`,
  `tools::observed*` and `tools::core` passed (849 tests; three delegation
  fault-injection tests that need `--test-threads=1` passed when rerun
  serially); `cargo test --bin nib doctor:: auth::` passed.

### Phase 2: user config layering and the project-preference boundary (2026-10-24)

- `src/config/layers.rs` builds the effective configuration from, in rising
  precedence, the user configuration `NibPaths::user_config_file()`
  (`~/.nib/config.toml`, `NIB_HOME` overrides), the project's preferences
  `<project>/.nib/config.toml`, and the user's settings for the project
  `~/.nib/projects/<project-key>/config.toml` (`ProjectSettings`). Provider
  environment variables keep their precedence where they are read.
  Interpretation of Rule 2: the project's preferences sit above the user
  configuration so a project can select its own provider and model over the
  user's defaults ("model/provider selection among user-configured
  providers"); since they can hold no user-only key, nothing they set can
  override a credential, endpoint or safety value.
- User-only keys (`USER_ONLY_PROVIDER_KEYS` = `api_key`, `api_keys`,
  `base_url`, `api`, plus `[approvals]`, `execution.sandbox` and
  `[workspace]`) are split out of every project document. Saves and edits
  that would put them in the project (`update_nib_config`,
  `edit_nib_config`, `preferences_document`) are rejected with a diagnostic
  naming the keys; `save_config` and `save_nib_config_full` route them to
  the project's user settings instead.
- Decision 3: the approvals preset (`/permissions <mode>`), the sandbox
  level and the startup directory grant (`grant_workspace_access`) are kept
  per project in the user home. `NibPaths::project_key` is the folder name
  plus a SHA-256 prefix of the canonical root; a linked session or subagent
  worktree resolves to its main checkout (the check moved from
  `permission_rules` to `NibPaths::project_identity_root`).
  `projects/<key>/project.toml` records the canonical root for phase 5
  re-linking. New user-home directories are `0700` and files `0600`.
- `nib auth` writes only `~/.nib/config.toml` (`update_user_config_in`); a
  project still on the placeholder `mock` provider drops that selection so
  the configured provider applies. `nib doctor --fix` writes the OpenAI
  transport repair to the user layer that holds the transport, and doctor
  prints the user config and project settings paths.
- Legacy migration of the project configuration (the config part of Rule 6,
  needed so phase 2 does not break existing projects): on a locked load an
  untracked project file holding user-only values is (1) copied byte for
  byte to `<project>/.nib/legacy-<time>/config.toml` (an identical earlier
  copy is reused), (2) merged into the user's project settings without
  replacing a different existing value (a conflict stops with a diagnostic
  and changes nothing), with a `migration` record, and (3) rewritten as
  preferences only, under the expectation that it is still the file read.
  Each step is resumable. A file tracked by Git is never migrated: real
  user-only values are refused, default-valued keys ignored. After the
  migration, user-only values planted again are refused. Read-only loads
  accept unmigrated values only if they are already in the user settings.
  Legacy `config.json` goes through the same path after its TOML conversion.
- Test isolation: unit tests resolve the user home to a per-thread
  `TestNibHome` or a process-wide temporary home; `.cargo/config.toml` sets
  `NIB_HOME=target/test-nib-home` for every cargo-run process (matching
  `task test`), so binary and integration tests never touch `~/.nib`.
- The project-key Git probe runs `git -c core.fsmonitor=false` so an
  untrusted repository's configuration cannot start a helper.
- Validation: `cargo fmt`, `task check` passed; `cargo test --lib
  -- --test-threads=2` passed 1588 of 1590 (including 17 new
  `config::layers` tests), and the two delegation fault-injection tests
  that need serial execution passed with `--test-threads=1`;
  `cargo test --bin nib` (114 passed:
  (auth wizard: user config only, `0600`, placeholder switch, corrupt
  project config fails closed; doctor transport repair keeps the project
  untouched and is idempotent) and the `config_integrity`,
  `config_migration` and `doctor_cli` integration targets passed. The
  `docs_integrity` spec-field check passes for this spec (the "Phased
  Delivery" heading now names the implementation plan).

### Phase 3: runtime state in the user home (2026-10-24)

- `NibPaths::project_state` resolves to `~/.nib/projects/<state key>`
  (`NibPaths::user_state_dir`). The state key is the folder name plus a
  SHA-256 prefix of the folder's own canonical root (`NibPaths::state_key`);
  for a main checkout it equals the phase 2 project key, so settings and
  state share `projects/<key>/`. Exceptions, all documented on the function:
  - a worktree nib manages (`<state>/worktrees/{sessions,subagents}/<name>`,
    in the user home or a legacy `.nib`) keeps its state inside itself, as
    before, so it goes away with the worktree. Subagents therefore keep the
    explicit in-worktree `state_dir` of `delegation/part_06.rs` (a deviation
    from the Exclusions bullet, which would have mixed every subagent's
    memory and daemons into the parent's);
  - a Git worktree the user created keeps state of its own (its folder is a
    separate working tree with its own subagents and processes); its
    settings stay its main project's (phase 2);
  - a project whose `.nib` still holds legacy runtime state and has no state
    record keeps using `.nib` until the migration moves it, so state is
    never split between two places;
  - with no user home or no resolvable folder the in-project `.nib` is used.
- Default profile state (`memory.json`, `context.json`, `daemons/`,
  `managed-skills/`, the T044 marker) moved to `<state>/profiles/<id>`.
  Sessions and what is derived from their directory (checkpoints, read
  ledgers, presence) stay in `<project>/.nib/profiles/<id>/` until phase 4.
  A configured `state_dir` is unchanged. `Profile` keeps the scopes each
  directory must stay inside.
- New session and subagent worktrees are created under
  `<state>/worktrees/`. Legacy worktrees under `<project>/.nib/worktrees`
  (decision 1) stay manageable: `NibPaths::managed_worktree_roots` (and the
  session/subagent variants) are accepted by the ownership receipt check,
  `validate_record_worktree`, the reconciled-namespace proof and doctor.
  Creation checks are rebased from "inside the repository" to "inside the
  project state", and the delegation record, owner-lease and lock checks
  likewise.
- The sandbox mount plan recognizes both layouts. For a worktree in the user
  home the project is the root recorded in `<state>/project.toml`, accepted
  only when that root's state key names the directory (never the
  worktree's writable `.git` file). nib's user home (wherever `NIB_HOME`
  points) is masked for every sandboxed command and the managed worktree is
  bound again inside it; commands cannot start inside the user home.
- `src/state_migration.rs` moves the legacy entries
  (`NibPaths::LEGACY_STATE_ENTRIES`: `command-prefixes.json`, `daemons`,
  `memory.json`, `plans`, `process-scopes`, `subagent-owner-leases`,
  `subagents`, `worktree-ownership`, and per profile
  `LEGACY_PROFILE_STATE_ENTRIES`) when `ProfileRegistry::load` runs, under a
  file lock in the state directory, then re-resolves the profiles:
  - entries are renamed, never copied: content and the file identities
    that subagent receipts (`records_identity`) and locks are bound to
    survive. A probe rename first; a user home on another file system defers
    the migration with a message instead of stopping halfway. Rename uses
    `renameat2(RENAME_NOREPLACE)` on Linux;
  - before anything moves, a differing destination stops the migration
    with a diagnostic naming both paths; an identical file (an interrupted
    move) has its legacy copy kept in `<project>/.nib/legacy-<time>/state/`;
  - a live session heartbeat, an owner lease or a recorded process scope
    defers the migration, and the project keeps resolving to `.nib`;
  - `state.toml` (status `moving`, then `done`, with the moved entries and
    kept copies) is written before the first rename, so the project resolves
    to the user home from then on and an interrupted run resumes;
  - "state moved" marker: the T044 marker path of each default-state
    profile receives a version 2 marker naming the new location. Older
    versions parse it, reject the version and stop at profile loading,
    before they touch any state; this version ignores it. Legacy entries an
    older version writes anyway are left in place and reported, never
    merged over the moved state.
  - a symbolic link among the legacy entries stops the migration.
- Lock anchors of user-home state follow the general rule (the parent of
  the locked directory, here `~/.nib/projects/`); the `.nib`-named special
  case of `daemons/state/split_01.rs` now applies only to legacy in-project
  and in-worktree state. Directory capabilities that were opened from the
  project root (process scopes, records, owner leases, merge locks,
  delegation locks, subagent worktree parents) are opened from the state
  directory's parent.
- New user-home directories are `0700` and records `0600`
  (`ensure_state_directory`, `ensure_state_parent`).
- Unit tests: the process-wide test home is passed to child processes the
  tests spawn from the test binary (`NIB_UNIT_TEST_PROCESS_HOME`), so they
  share the parent's state; tests that built legacy state paths by hand now
  resolve them through `NibPaths`.
- Validation: `cargo fmt`, `task check`; focused tests below.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-goal-minor | Persisted state layout, storage backend and config location change with automatic migration; shares the goal minor 0.6.0 from published 0.5.2. |

## Memory Impact

Status: pending
Rationale: The decisions recorded above are durable; they are written to project
memory when this spec is verified.
