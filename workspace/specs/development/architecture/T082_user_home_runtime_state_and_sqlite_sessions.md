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
  [T080](../../test/tools-sandbox/T080_workspace_sandbox_and_permission_modes.md)), and has
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
5. **Worktrees outside the project.** Under [T080](../../test/tools-sandbox/T080_workspace_sandbox_and_permission_modes.md),
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

- [x] AC-1: A fresh project using the new version creates no runtime state or
  secrets under `<project>/.nib`. Only preference files may appear there.
  Evidence (phases 3 and 4): runtime state lives in `~/.nib/projects/<key>/`
  and sessions in `~/.nib/nib.db`
  (`state_migration::tests::a_fresh_project_records_its_state_in_the_user_home`,
  `session::database::tests::default_user_home_sessions_live_in_the_database_and_others_in_files`);
  `doctor::tests::doctor_accepts_healthy_default_runtime` runs doctor (which
  records an audit session) on a fresh project and finds no
  `.nib/profiles` or `.nib/sessions`. Credentials stay in the user layers
  (AC-2).
- [x] AC-2: `nib auth` stores credentials only in `~/.nib/config.toml` with
  mode 0600. Project config that sets credentials, endpoints, approvals,
  sandbox or trust is rejected with a diagnostic. Evidence (phase 2):
  `auth::tests::auth_wizard_accepts_numeric_provider_and_one_shot_mode`
  (user config only, mode `0600`, no secret in the project),
  `config::layers::tests::a_shipped_project_file_cannot_redirect_credentials_or_relax_safety`,
  `project_preference_edits_cannot_set_user_only_values` and
  `user_values_added_after_the_migration_are_rejected`. Existing untracked
  legacy files are migrated instead of rejected (see phase 2 notes).
- [x] AC-3: Sessions are persisted in `~/.nib/nib.db`. Concurrent writers are
  rejected on revision conflict, and run leases are exclusive across
  processes. Existing session tests pass against the SQLite backend.
  Evidence (phase 4): `session::database::tests` (10):
  `a_stale_snapshot_is_rejected_by_the_revision_guard` (store-level stale
  snapshot and the row guard below the locks),
  `database_sessions_are_guarded_across_processes` (a second process sees
  the run lease held and its stale write is rejected),
  `a_failing_commit_check_rolls_the_whole_write_back`,
  `appending_a_message_writes_only_the_new_row`, round trip with exact
  floats, delete, corrupt rows, newer schema refused, re-link. Every store
  opened through a profile (`for_project`, the agent loop, gateway, MCP,
  TUI, delegation audits, durable tasks, curator) now uses the database, so
  their existing tests run against it (`cargo test --lib`: 1622 of 1623
  with two threads; the remaining timing-sensitive presence test passes on
  rerun, and the delegation fault-injection tests pass serially as before). The session-directory tests that construct a store
  in a temporary folder keep exercising the JSON file backend, which stays
  for configured `state_dir` profiles and managed worktrees.
- [x] AC-4: The migration imports every existing JSON session (count and
  content parity checked), moves runtime state and credentials, keeps the
  originals in the legacy directory, and is idempotent and resumable after
  interruption. Evidence (phases 2 to 4): from a populated legacy layout
  (`state_migration::tests::a_populated_legacy_layout_moves_to_the_user_home_without_loss`:
  every readable session in the database equal to its file, the original
  `sessions/` folder with an unreadable file, locks and identity marker
  kept byte for byte in `.nib/legacy-<time>/sessions/`, state moved with
  directory identity kept), `a_second_run_changes_nothing`,
  `an_interrupted_session_import_resumes_without_duplicates`,
  `an_interrupted_migration_resumes_and_keeps_identical_leftovers`,
  `a_session_the_database_holds_differently_stops_the_first_migration`,
  `a_session_run_in_progress_defers_the_session_import`,
  `pre_profile_sessions_reach_the_session_database`,
  `a_recorded_legacy_session_directory_reaches_the_imported_sessions`,
  `a_run_stopped_after_the_last_rename_is_finished_by_the_next_one` (a run
  stopped after the session folder's rename: the record already names the
  import, the next load finishes it, and the recorded legacy path reaches
  the database without a store in the project),
  `a_session_saved_between_its_import_and_the_rename_is_reported`,
  `a_session_directory_over_the_entry_limit_stops_the_migration`;
  credentials and safety values move with the configuration migration
  (phase 2, AC-2). Runtime state moves by rename, so no second copy of a
  moved entry exists; identical leftovers and all session originals are
  kept in the legacy directory.
- [x] AC-5: Copying or moving the project directory does not break nib.
  `nib doctor` detects the mismatch and `--fix` re-links with confirmation.
  Evidence (phase 5): a copy keeps its own state and the original is never
  re-linked (`state_migration::relink::tests::a_copy_keeps_its_own_state_and_is_never_relinked`);
  a moved project is found and re-linked with its sessions and memory, its
  own newer state kept aside
  (`a_moved_project_is_found_and_relinked_with_its_sessions_and_memory`),
  refused while the old state is in use
  (`relink_waits_while_the_old_state_is_in_use`), and an interrupted
  re-link finishes on the next profile load
  (`an_interrupted_relink_finishes_when_the_profiles_load`). End to end,
  `doctor_cli::doctor_cli_reports_a_moved_project_and_relinks_its_sessions`
  runs the binary: doctor reports "Moved?" with the command, and
  `nib doctor --fix --relink <old folder>` (naming the old folder is the
  confirmation) moves the session. Doctor passes on a copied or moved
  project because receipts are keyed by folder.
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
  Progress (phase 5): doctor prints "Checking nib layout" (user home,
  `nib.db` and this folder's session count, runtime state, older files
  left in `.nib`, moved or copied projects); the user guide covers the
  layout, the session import, re-linking and rollback; the governed
  `backend_rust.md` records the bundled `rusqlite` decision (scoped
  approval). Open: the native macOS and Windows CI runs (only Linux was
  built and tested locally), catalog, versions and memory reconciliation
  (coordinator records), and the mandatory independent review.

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
- **Rollback.** Runtime state moves by rename, so the legacy directory
  holds only the original sessions and identical leftovers. Going back to
  an older binary therefore means: stop nib, delete the version-2 "state
  moved" markers (`.nib/profiles/<id>/.legacy-state-migration-v1.json`),
  move the entries `state.toml` lists under `moved` from
  `~/.nib/projects/<key>/` back into `.nib/`, and move
  `legacy-<time>/sessions/profiles/<id>/sessions/` (with its lock anchors)
  and `legacy-<time>/sessions/sessions/` back. Sessions created or changed
  after the migration exist only in `nib.db`, which is kept. The user guide
  lists these steps. Published versions are never reused.

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
- Sessions and durable tasks: code that derived a profile's daemon
  directory from its sessions directory (`DurableTaskStore::from_sessions_dir`
  and the timer audit in `tools/core.rs`) now resolves the owning profile
  (`DurableTaskStore::profile_daemon_dir`), falling back to the sibling
  directory for sessions outside a configured workspace. The owner lease
  namespace lock is derived from the state directory itself.
- `NIB_HOME` joins the environment allowlist for child processes
  (`sandbox::INHERITED_ENVIRONMENT_ALLOWLIST`), so a nib started by a tool or
  as an MCP server uses the same user home. Without it, integration tests
  that start `nib mcp-server` through an MCP manager wrote their temporary
  projects' state into the developer's `~/.nib` (found during this phase,
  removed, and covered by `sandbox::tests::child_environment_keeps_nib_home`).
- Worktree creation records the project identity
  (`state_migration::ensure_state_identity`), so the sandbox can trace a
  user-home worktree back to its project even before the first profile load.
- Validation (2026-10-24): `cargo fmt`; `task check` passed; `cargo test
  --lib -- --test-threads=2` passed 1602 of 1604, and the two delegation
  fault-injection tests that share injection counters
  (`expired_spawn_preparation_retains_intent_until_fresh_restart_cleanup`,
  `partial_cleanup_failures_retain_intent_until_restart_reconciles_last`)
  passed with `--test-threads=1`, as in phase 2. New tests:
  `state_migration::tests` (12: populated legacy layout moved without loss
  and with preserved directory identity, idempotence, interrupted-run
  resumption with identical leftovers kept, conflict before any change,
  deferral while a session, owner lease or process scope is live,
  configured state directories, symbolic links, the state-moved marker and
  reappearing legacy state, fresh projects, managed worktrees, profile
  loading), `sandbox::project_mounts_tests::user_home_worktrees_are_managed_and_the_user_home_stays_hidden`
  (bwrap: Git works in a user-home worktree, the user home and the
  project's `.nib` stay hidden) and `child_environment_keeps_nib_home`.
  Integration targets `delegation`, `executor`, `durable_tasks`,
  `doctor_cli`, `config_integrity`, `config_migration`, `test_runtime_e2e`,
  `mcp_integration`, `managed_process_*`, `session_roundtrip`,
  `interactive_cli` and `sandbox_integration` passed, and no test run left a
  `~/.nib` behind. `docs_integrity` passes for this spec (it still fails for
  T087, outside this spec).

### Phase 4: SQLite sessions and the session import (2026-10-24)

- `rusqlite` 0.41 with only the `bundled` feature (decision 4); the scoped
  amendment to `workspace/instructions/tech/backend_rust.md` records it.
  `Cargo.lock` gains `rusqlite`, `libsqlite3-sys` and their small
  dependencies (a necessary change inside the records boundary; the package
  version is untouched).
- `src/session/database.rs`: `~/.nib/nib.db` (`NibPaths::session_database`,
  created `0600` without following links), WAL, `synchronous=FULL`, a 30 s
  busy timeout, foreign keys, and `schema_version` migrations in a
  transaction; a newer schema is refused. Tables: `sessions` (scope = the
  session directory relative to the user home, id, `project_key`,
  `profile_id`, `revision`, `generation`, the session document without
  messages and events, stored size, change time), `session_messages` and
  `session_events` (one row per entry, keyed by session, indexed by
  project, cascading), and `session_imports` (the import ledger).
  Connections are pooled per database path.
- `SessionStore` keeps its API. A store whose directory is the default
  session directory of a user-home state
  (`<user home>/projects/<key>/profiles/<id>/sessions`,
  `NibPaths::database_session_scope`) reads and writes the database; any
  other directory (a configured `state_dir`, a managed worktree, a project
  whose state has not moved yet, test folders) keeps JSON files. The
  directory still exists for the locks, run leases and identity marker, and
  its siblings hold checkpoints, read ledgers and presence.
- Writes: one `IMMEDIATE` transaction guarded by the row the caller read
  (`UPDATE ... WHERE generation = ? AND revision = ?`, or an insert that
  fails on an existing row); message and event rows are appended, and an
  earlier row is rewritten only when the session changed it (event
  reconciliation of older audit evidence). The commit check runs inside the
  transaction and can abort it. The transaction replaces the file store's
  read-back verification. Deletes, subagent audit preparation (the row's
  `generation` is the receipt, `SessionPreparationReceipt::session_generation`)
  and its cleanup (also after a restart, `remove_prepared`) use rows the
  same way; a namespace another spawn adopted is recognized by its other
  rows.
- Deviation from Rule 3: the striped lock files and the file-lock run
  lease stay. The locks serialize the store's closure-based
  read-modify-write calls across processes without holding a database-wide
  write transaction while caller code runs (the revision guard is the
  optimistic check at commit), and a kernel file lock is released when its
  process dies, which a PID/heartbeat row would only approximate.
- The session size limit (64 MiB) applies to the stored compact JSON.
- Deviation from Rule 4: sessions are scoped per project and profile (each
  store lists only its scope; `session_counts` groups by project); a listing
  across all projects is left for a follow-up.
- Import (`src/state_migration_sessions.rs`, part of the state migration):
  every legacy `.nib/profiles/<id>/sessions/*.json` is read (bounded,
  `O_NOFOLLOW`), parsed, validated and compared with the database before
  anything changes (`import_status`: the ledger's SHA-256 or an identical
  row means imported; a different row is a conflict). A conflict stops the
  first migration with a diagnostic; once the state moved it leaves that
  folder in place and reports it. Each session is imported in its own
  transaction with its ledger entry and the file's change time; then the
  whole folder (session files, files nib could not read, locks and identity
  marker) and its lock anchors are renamed into
  `.nib/legacy-<time>/sessions/profiles/<id>/`. A held run lease of any
  session (probed with a non-creating `try_lock`) postpones the migration.
  `checkpoints`, `file_reads` and `presence` now move with the rest of the
  profile state, and `.nib/profiles/<id>/sessions` and pre-profile
  `.nib/sessions` count as legacy state. Pre-profile sessions reach the
  database through the profile migration, which imports instead of copying
  when the profile's sessions are in the database.
- Session directories recorded before the import (durable tasks) resolve
  to the imported location (`state_migration::moved_sessions_dir`, used by
  `SessionStore::at_dir`, the durable task profile resolution and the
  worker check), so no second store starts in the project. Sessions in the
  user home resolve back to their project through `project.toml`
  (`NibPaths::state_owner`).
- `ProfileRegistry::load` re-resolves the profiles whenever the migration
  changed where the state resolves (also when it only recorded it), and a
  "state moved" marker a copied project carries along is not legacy state.
- `SessionStore::database_path` and `SessionStore::session_exists_at`
  (read-only) are new public helpers; `delete_if` and
  `load_result_with_metadata` pass a `SessionRecordMetadata` (modification
  time and size of the file or row) instead of `fs::Metadata`.

### Review fixes (2026-10-24)

An independent review of `fefb5ed` requested changes. Every finding was
fixed; one part of a minor finding is recorded as a deviation.

- Major, interrupted migrations: `state.toml` is now written before every
  entry move, session folder rename and pre-profile retirement (an import
  is recorded before its rename; `moved_sessions_dir` only redirects once
  the folder is gone), and a run that finds nothing left finishes a record
  still `moving` (markers, `done`, `completed_at`). Test:
  `a_run_stopped_after_the_last_rename_is_finished_by_the_next_one`.
- Directory modes: `open_session_directory` creates directories through
  `state_migration::ensure_state_directory`, so a store opened on a
  user-home path that does not exist yet creates every directory `0700`
  (`session::database::tests::a_session_directory_in_the_user_home_is_created_private`,
  which fails without the fix). The fresh-project test checks only the
  directory it creates, so it no longer depends on test order;
  `cargo test --lib -- session::database state_migration` passes.
- Writers between import and rename: the cheaper fix. After the rename the
  kept folder is hashed again; session files that changed or appeared since
  the plan are recorded in `state.toml` (`not_imported`, with the
  unreadable files), logged, and listed by `nib doctor` ("Not imported",
  a warning). Their content is not merged into the database, whose row may
  already have newer writes. Test:
  `a_session_saved_between_its_import_and_the_rename_is_reported`.
- Deviation from Rule 6 ("recorded in `nib.db` and confirmed in the TUI"):
  the migration is recorded in `state.toml` and in the database's import
  ledger, and its outcome is reported in the log; there is no TUI
  confirmation. A migration only renames and imports (nothing is deleted),
  and `nib doctor` lists what is left, the legacy copies and every file not
  imported. A TUI notice is left for a follow-up.
- Rollback: the guide and Risks and Rollback now list the actual steps
  (markers, moved entries, session folders; sessions after the migration
  are only in `nib.db`).
- Pre-profile `.nib/sessions`: once a profile migration marker shows it
  was imported, the folder and its lock anchors are moved into
  `legacy-<time>/sessions/sessions/` (a held run lease in it postpones
  this), so the project stops reporting legacy state
  (`pre_profile_sessions_reach_the_session_database`).
- Info: `nib doctor` lists states kept aside by a re-link
  (`projects/.relinked-<key>-<time>`) with the `--relink <aside key>`
  command that brings one back (asserted in
  `a_moved_project_is_found_and_relinked_with_its_sessions_and_memory`);
  `relinked-<time>/` is created through `ensure_state_directory`.
- Info: a legacy session folder with more than `MAX_SESSION_DIR_ENTRIES`
  (100,000) entries stops the migration with a diagnostic before anything
  changes instead of being renamed with unplanned files
  (`a_session_directory_over_the_entry_limit_stops_the_migration`, with a
  limit of 64 under test).
- Validation (2026-10-24): `cargo fmt`; `task check` passed; `cargo test
  --lib -- --test-threads=2` passed 1627 of 1627; `cargo test --lib --
  session::database state_migration` passed 35 three times in a row;
  `doctor_cli` (7), `session_roundtrip` (17) and `durable_tasks` (4)
  passed.

### Phase 5: doctor, re-linking and documentation (2026-10-24)

- `nib doctor` prints "Checking nib layout": the user home, `nib.db` with
  this folder's session count, the runtime state directory (and whether its
  move is pending or was interrupted), what older versions left in `.nib`
  (runtime state not moved yet, `legacy-<time>` folders, older worktrees),
  and other states this folder may belong to. These lines warn and never
  fail the doctor. The sessions line names the database.
- `src/state_relink.rs` (`nib::state_migration::relink`): candidates are
  state directories whose recorded root no longer exists (`Moved` with the
  same folder name, else `Orphaned`), still exists with the same name and
  Git root commit (`Copied`, reported only), or a re-link to this folder
  that was interrupted.
- `nib doctor --fix --relink <old folder or key>`: naming the old folder is
  the confirmation (no prompt, scriptable). Under a lock in
  `~/.nib/projects`, it refuses when the old folder still exists (a copy)
  or the old or new state is in use, writes a journal (`relink.toml`)
  before the first change and after each step, keeps a state the new
  folder already has (with its sessions, moved to the aside key) in
  `projects/.relinked-<key>-<time>`, keeps worktree receipts naming the
  old folder in `relinked-<time>/`, points `project.toml` and `state.toml`
  at the new folder, renames the state directory, moves the sessions to the
  new key (`SessionDatabase::relink_project`, refused on an id present
  under both keys), and runs `git worktree repair` for the managed
  worktrees. An interrupted re-link finishes on the next profile load or
  `--relink`. Nothing is deleted.
- User guide: the layout (database, session directory), the session import
  and legacy copies, moving and copying projects, rollback. The `/land`
  messages name the worktree folder in nib's state instead of
  `.nib/worktrees/sessions/` only.
- Validation (2026-10-24): `cargo fmt`; `task check` passed; `cargo test
  --lib -- --test-threads=2` passed 1622 of 1623 (the timing-sensitive
  `session::presence` heartbeat test failed once under load and passed on
  rerun; with `--test-threads=1` the presence tests and the two delegation
  fault-injection tests passed); `cargo test --bin nib`
  passed 114; integration targets `session_roundtrip`, `doctor_cli` (7,
  including the moved-project re-link), `config_integrity`,
  `config_migration`, `durable_tasks`, `executor`, `delegation`,
  `interactive_cli`, `compression_runtime`, `llm_failure_cli`,
  `managed_process_*` (Linux), `sandbox_integration`, `mcp_integration`
  and `test_runtime_e2e` passed. `docs_integrity` passes for this spec
  (it fails for T087, FT-021 and FT-022, outside this spec). No test run
  left a `~/.nib` behind.

### Gate fixes (2026-10-24)

- `test_runtime_e2e::mcp_delegation_is_permission_gated_dispatched_and_audited_over_stdio`
  failed with `audit recording failed: ... state directory changed`. The
  test audited into `SessionStore::new(root)`, the pre-profile
  `<project>/.nib/sessions` layout. The `nib mcp-server` child it spawns
  runs the state migration on its first stateful tool call
  (`nib_get_status`), which imported that directory into `nib.db` and
  moved it to `.nib/legacy-<time>/` while the parent still held it open;
  the parent's next audit write then failed closed. Runtime code never
  opens that layout (it opens `SessionStore::for_project`, which migrates
  first), so the test, not the product, encoded the old layout: it now
  audits into `SessionStore::for_project` (the database) and asserts that
  no `.nib/sessions` or `legacy-*` copy appears.
- `scripts/check-interactive-release.sh`: the smoke now sets `NIB_HOME`
  inside its fixture (it isolated `HOME` only, so an inherited `NIB_HOME`
  leaked through), checks the workspace consent in the user's project
  settings (`$NIB_HOME/projects/<key>/config.toml`) and that the
  project's `.nib/config.toml` has none, reads sessions through a
  read-only `python3` export of `nib.db` (one pretty JSON document per
  session) instead of `.nib/profiles/default/sessions/*.json`, and also
  scans `nib.db*` for the private sentinel. `tests/installers.rs` pins
  these contracts. The Windows smoke (`check-interactive-release.ps1`)
  still reads the old session and consent paths; it is not run by CI on
  this branch and needs the same change.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-goal-minor | Persisted state layout, storage backend and config location change with automatic migration; shares the goal minor 0.6.0 from published 0.5.2. |

## Memory Impact

Status: pending
Rationale: The decisions recorded above are durable; they are written to project
memory when this spec is verified.
