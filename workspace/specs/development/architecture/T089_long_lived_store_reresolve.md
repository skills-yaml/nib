# T089: Re-resolve a Held Store After State Migration

**Status:** Development. Started on 2026-10-25 as the recorded follow-up to
[T082](T082_user_home_runtime_state_and_sqlite_sessions.md). Implementation is
active on `fix/t088-store-reresolve` and is not yet integrated into the shared
`development` branch.
State: development
Primary Feature: architecture

## Problem

T082 moves a project's runtime state from `<project>/.nib` to `~/.nib` when
the project is idle. A process that publishes no presence (`nib mcp-server`,
the daemons) can open that legacy store while migration is deferred. After
another process migrates, the holder's next write fails closed with
`state directory changed` from `StableDirectory::verify_visible_at`. Nothing
is corrupted, and the process stops writing, which splits later work away
from the migrated store until the process is restarted.

## Choice

Re-resolve the store and perform the write once on the live location (option
a). Do not publish presence for the life of the process (option b).

Presence would defer migration for as long as an MCP server or daemon stays
up, so a TUI or `nib run` in the same project could not move the state. The
product goal is that those long-lived processes keep working when that move
happens. Re-resolve matches that goal and reuses the migration record T082
already writes. `ProfileRegistry::load` re-resolves a profile when that
record changes the state location; `SessionStore::at_dir` already follows
`moved_sessions_dir` for a path opened after the import. A held store uses
that same record. It does not call `ProfileRegistry::load` on the write
path, because load migrates when the project is idle and this process must
not start a migration.

## Scope

1. A held session store whose legacy session directory is gone, and whose
   import is recorded in `state.toml`, opens the user-home session directory
   once and performs that call's read or write there.
2. A held durable-task store whose daemon directory was renamed (same
   directory inode) accepts that directory in `verify_visible_at` and takes
   its file locks on the renamed path, so the write continues through the
   already-open directory.
3. A retry inside one call never runs. The call either uses the store it
   already holds, or switches to the migrated store before any lock or
   publication, and then runs the body once.
4. A visibility failure that is not a completed migration still fails closed.
   That includes a symlink placed at the opened path, a directory replaced
   at the same path, a deleted directory with no completed migration record,
   and a migrated path whose replacement is itself unsafe.

Out of scope: publishing presence; changing who may migrate; T082's
editor-smoke fail-fast; T086 narrow-terminal clipping; T080's MCP-server
worktree test; T087's guard minors; T023 and live evaluation; release to
`main`. An idle pre-T082 binary that recreates a legacy directory after
migration is unchanged and stays a release-note item.

## Proposed Behavior

Session directory. Before a session call takes a lock or writes, if
`symlink_metadata` of the opened session directory is not found and
`moved_sessions_dir` returns the imported user-home directory, open that
directory with `SessionStore::at_dir` and run the call on that store. The
opened path must not be a symlink. If that store's directory is also gone,
the call fails closed. Lock deadlines, timeouts, and sensitive values are
kept. The body runs once, so a save that already published is not applied
again.

Daemon and task directories. Entries T082 renames keep their inode.
`verify_visible_at` still requires the opened path to be a real directory
when that path exists. When the path is missing, the opened directory's
identity matches the user-home path recorded for that relative entry, and
`state.toml` is `done`, visibility succeeds and the in-flight write finishes
through the original file descriptor. No second write is started. File locks
and the held store's audit log are opened on that same renamed directory,
because opening the missing legacy path would fail or, on the no-deadline
lock helper, recreate `<project>/.nib`. A counterpart whose inode does not
match the held directory is an error before either helper runs. The legacy
path is not created.

Anything else. A path that still exists is never treated as migrated, so a
symlink swap or a replaced directory returns the existing fail-closed error
and does not receive the write. A missing path with no `done` record and no
covering `moved` entry is not redirected.

## Affected Areas

- `src/state_migration.rs`: counterpart path of a renamed legacy entry.
- `src/daemons/state/split_00.rs`: `verify_visible_at` for that counterpart.
- `src/daemons/workload/split_00.rs`: task and admission locks after a rename.
- `src/session/split_02.rs`: session reads and writes on a held store.
- `src/store_relocation_tests.rs` and `Taskfile.yml`: the focused regression.
- `workspace/releases.json`: T089 joins the already-applied `nib-all-provider-models`
  release. Cargo.toml, Cargo.lock, and skills.yaml stay at 0.7.0.

Consumers are session persistence, durable task preparation and updates,
and the daemons and schedule worker that hold those stores. MCP tool calls
that already call `ProfileRegistry::load` per call are unchanged. Config
saves and delegation records that fail closed on a replaced directory stay
fail-closed. No persisted format changes.

## Acceptance Criteria

- [ ] AC-1: A session store and a durable-task store opened while migration
  is deferred still read and write the legacy directories. After another
  call migrates the project, the held stores' next write lands once in the
  user-home session database and the renamed task directory. The legacy
  session and daemon paths are not recreated. Events and the task record
  are not duplicated. The held store's audit line is appended once under
  the renamed daemon directory. Evidence:
  `state_migration::store_relocation_tests::held_stores_follow_a_migration_without_losing_or_duplicating_writes`.
- [ ] AC-2: Replacing the opened directory with a symlink, or replacing it
  with a new directory at the same path, still fails closed and leaves the
  replacement unchanged. A missing directory with no completed migration
  record is not redirected onto `~/.nib`. Evidence:
  `symlink_and_replacement_still_fail_closed` and
  `missing_session_directory_without_a_migration_record_is_not_redirected`
  in the same module. The replaced task directory gains no lock file, and
  the daemon directory's names do not change.
  `unsafe_or_foreign_migrated_daemon_directory_does_not_recreate_legacy`
  covers a symlink and a different inode at the migrated daemon path.
  The existing config and delegation fail-closed tests are named under
  Validation Gates.
- [ ] AC-3: One call does not run its write twice. An unsafe migrated
  session directory fails the call without writing the new event and
  without recreating the legacy directory. Evidence:
  `unsafe_migrated_session_directory_does_not_receive_a_second_write`,
  and the single `stopped` / `compensation_failed` counts in the AC-1 test.
- [ ] AC-4: `task versions:check` accepts T089 as a patch member of the
  already-applied 0.7.0 release. `task check` and `task verify` pass, and
  the interactive and managed-process release smokes pass on a debug
  binary whose identity is the committed head. Evidence: pending until
  those gates run on the frozen candidate.

## Validation Gates

Changed-component tests first: `task test:store-reresolve`. Then the
existing fail-closed consumers
`config::tests::config_save_fails_closed_when_nib_directory_is_replaced` and
`tools::delegation::tests::test_part_1::spawn_forward_mutations_stop_at_detached_preparation_authority`.
After those pass, `task check`, then `task verify` with
`NIB_REQUIRE_BWRAP_TESTS=1`, then `cargo build` and
`scripts/check-interactive-release.sh` plus
`scripts/check-managed-process-release.sh` on that debug binary. Rebuild
the binary after the final commit because the smokes require it to match
HEAD. Integration is a pull request into `development`; this spec stays in
development until that merge is confirmed. Windows CI is skipped on
`development`.

## Risks

- A write that already published and then lost its directory must not be
  repeated. The switch happens before the call takes a lock or publishes,
  and the body runs once. Same-inode task writes keep the original file
  descriptor, so visibility success is not a second publication.
- Following a missing path onto a different inode would split or duplicate
  state. The task path accepts a counterpart only when the directory
  identity matches. Sessions are imported, not renamed, so identity match
  does not apply to them; they reopen the recorded session directory.
- `with_file_lock_in` creates a missing parent. Session calls must leave
  the legacy path before that helper runs. Task locks verify the held
  directories before `TaskLock::acquire`. An unsafe or different-inode
  counterpart is an error before that helper, so a missing legacy path
  is not created. The path a successful lock opens is the renamed directory.
- An identical file that T082 kept aside under `legacy-<time>/state/` has
  a different inode from the user-home copy. The held descriptor does not
  match, and the write stays fail-closed.
- Publishing presence was rejected because it would pin the project in the
  legacy layout for the process lifetime.

## Implementation Plan

1. Join the already-applied `nib-all-provider-models` release at 0.7.0.
   Development moved from published 0.6.0 to that open minor while this
   work was in progress. A second open reservation is not allowed, so
   0.6.1 is not applied.
2. Add the migration-counterpart lookup and teach `verify_visible_at` and
   task lock acquisition to use it only for a missing path with a matching
   inode.
3. Re-open a held session store through `moved_sessions_dir` and
   `SessionStore::at_dir` before any session lock or mutation.
4. Add deterministic tests that simulate the deferred holder, the migrating
   process, the successful next write, symlink and replacement failures,
   and a write that is not repeated.
5. Obtain an independent exact-candidate review, fix every finding, and run
   the validation gates above.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-all-provider-models | Joins the already-applied 0.7.0 reservation. No second bump. Compatible recovery for a held store after T082 migration. |

## Memory Impact

Status: pending
Rationale: The re-resolve rule is durable, and it is written to project
memory when this spec is verified. It stays pending until then.
