# T029: Explicit Self-Update Channel Switching

**Status:** Development
**Related:** [FT-018](../done/ft_018_self_update_and_update_notifications.md), [T010](../done/T010_release_process.md)

## Summary

Allow an installed official nib release to switch between the production and
development rolling channels with an explicit `nib update --channel <channel>` request.
The selected release binary becomes the durable channel choice because its verified
embedded build identity controls later startup checks and ordinary `nib update` calls.

## Problem Statement

`nib update` currently follows only the channel embedded in the installed binary. A
user who wants to move between `prod-latest` and `development-latest` must discover and
rerun the platform installer. The installer remains necessary for unmanaged or
package-manager-owned builds, but an already managed writable nib installation has all
the integrity, smoke, locking, and replacement machinery needed to perform an explicit
channel switch safely.

## Product Decisions

- `nib update` without options retains its existing within-channel behavior.
- `nib update --channel prod` and `nib update --channel development` explicitly select
  the target rolling channel. `production` and `dev` are accepted as CLI aliases, but
  output uses the canonical `prod` and `development` names.
- Channel selection is not project configuration and is not stored in `.nib`. The
  verified target binary's embedded channel is authoritative for subsequent updates and
  startup notices.
- Only official managed `prod` or `development` builds may switch channels. Local,
  source-built, unknown, unsupported, non-writable, or ambiguous installations retain
  the existing fail-closed installer guidance.
- An explicit switch installs the target-channel binary even when both rolling
  manifests name the same commit. This is required to change the embedded channel.
- A channel switch may move to a different or historically older exact commit because
  rolling channels intentionally do not define semantic ordering. The explicit
  `--channel` option is the user's authorization for that movement.
- The official repository, target channel tags, artifact set, redirect policy, and
  exclusive release writer remain compile-time controlled.

## Scope

- Add an optional typed `--channel` argument to the public `update` subcommand.
- Resolve a target channel from the explicit argument or the current embedded channel.
- Fetch and strictly validate the target channel's manifest, archive, and checksum.
- Classify a different target channel as an installable transition even when the exact
  commit and package version equal the current build.
- Verify that the staged executable embeds the requested target channel before any
  replacement commit point.
- Reuse the existing installation lock, path identity checks, safe extraction, staged
  smoke, Unix atomic replacement, and Windows cleanup handoff.
- Report a distinct successful channel-switch result and document how to switch back.
- Add deterministic CLI and updater regressions for selection, same-commit switching,
  aliases, invalid values, unmanaged builds, and unchanged within-channel behavior.

## Non-Goals

- Allowing local/source builds or arbitrary package-manager installations to replace
  themselves.
- Persisting a channel preference outside the installed binary.
- Automatically switching channels during startup checks.
- Adding custom repositories, tags, manifest URLs, or unsigned artifacts.
- Comparing channels by semantic version, ancestry, freshness, or stability.
- Changing the release workflow, protected environments, rolling publication
  transaction, or production approval policy.
- Adding an interactive confirmation prompt to the already explicit foreground update
  command.

## User Experience

Ordinary update within the embedded channel:

```text
nib update
```

Explicitly switch to development:

```text
nib update --channel development
```

Explicitly switch back to production:

```text
nib update --channel prod
```

Successful channel switch:

```text
Switched nib channel: 0.1.0 (prod, 0123456) -> 0.1.0 (development, 89abcde)
```

After replacement, `nib version` reports the target embedded channel. Later
`nib update` calls and startup availability checks follow that channel without another
option.

## Security And Reliability Requirements

- Reject unmanaged current-build identity before network I/O or filesystem mutation,
  including when `--channel` is present.
- Parse channel selection through Clap's bounded value grammar; never interpolate an
  arbitrary user string into a release URL.
- Validate the target manifest's canonical channel and rolling tag before downloading
  an archive.
- Download the archive and checksum from the same target channel as the validated
  manifest.
- Require the staged binary's repository-controlled version output to match the exact
  target version, channel, and commit before replacement.
- Preserve the current executable on every pre-commit error and retain the existing
  Windows recovery guarantees after the commit point.
- Keep startup checking read-only and bound to the channel embedded in the currently
  running binary.

## Acceptance Criteria

- [x] `nib update --help` documents `--channel <prod|development>` and its purpose.
- [x] `nib update` without `--channel` preserves the existing current-channel no-op and
      update behavior.
- [x] `nib update --channel development` from a managed production build validates and
      installs only the `development-latest` artifact set.
- [x] `nib update --channel prod` from a managed development build validates and
      installs only the `prod-latest` artifact set.
- [x] `production` and `dev` aliases resolve to the canonical channels, while unknown
      values are rejected by CLI parsing before updater execution.
- [x] A requested channel different from the embedded channel forces staged binary
      installation even when both manifests name the same commit and version.
- [x] A same-channel, same-commit request remains a successful no-op with no archive
      download or executable mutation.
- [x] Successful switch output names both exact build identities, and the next
      `nib version`, startup check, and option-free `nib update` follow the target
      embedded channel.
- [x] Local, source-built, unsupported, non-writable, concurrent, and ambiguous
      installations retain existing fail-closed behavior without channel preference
      persistence.
- [ ] Manifest, checksum, archive, extraction, staged-identity, race, or replacement
      failures cannot install a binary from the wrong channel or damage the current
      executable.
- [x] README and the end-user guide document switching, persistence through embedded
      identity, aliases, and the installer fallback for unmanaged builds.

## Affected Areas

- `src/main.rs` — typed update arguments and CLI dispatch.
- `src/updater.rs` — target-channel resolution, availability classification, target
  artifact routing, output, and deterministic regressions.
- `tests/updater.rs` — public help, parsing, and unmanaged-build behavior.
- `README.md` and `docs/user/guide.md` — channel-switch commands and safety boundary.
- `docs/tech/ci.md` — the embedded-channel update contract.
- `agents/memory/` — durable channel-switching decision and delivered behavior.

The release workflow, installer implementations, profile configuration, workload
model, sessions, LLM transports, tools, and TUI are not modified.

## Implementation Plan

1. Add typed update arguments with canonical production/development values and bounded
   aliases.
2. Separate current embedded identity from requested target-channel resolution.
3. Make availability classification channel-aware and route every target asset fetch
   through the requested channel.
4. Preserve the existing staged identity and platform replacement boundaries, then add
   distinct switch output.
5. Add unit and CLI regressions, update user/technical documentation, and run the
   focused plus canonical Task gates.
6. Publish an updater-capable development build and exercise a real managed
   cross-channel switch before moving this spec to `done`.

## Validation Gates

- Updater unit tests cover current-channel no-op, current-channel update,
  different-channel/different-commit transition, different-channel/same-commit
  transition, conflicting same-commit version metadata, canonical output, and target
  artifact routing.
- CLI tests cover help text, canonical channel values, aliases, invalid values, startup
  check exclusion, and unmanaged rejection before network access.
- Existing archive, checksum, staged-binary, lock, race, Unix replacement, and Windows
  cleanup/recovery tests remain green.
- `task test:updater`.
- `task docs:check`.
- `task check`.
- Independent `task test`.
- `task coverage` and `task build` before completion reconciliation.
- Exact-revision hosted Linux, macOS Intel, macOS Apple Silicon, and Windows CI.
- A real managed development build switches to production (or the inverse), reports
  the requested embedded channel, and performs a subsequent within-channel no-op before
  the spec moves to `done`.

## Risks And Mitigations

- **Unintentional rollback:** Development and production rolling channels can point to
  commits in either chronological order. Require the explicit `--channel` option and
  report both exact identities rather than claiming an upgrade.
- **Same-commit false no-op:** Comparing only commit identity would skip the binary
  replacement and leave the old embedded channel. Include channel equality in the
  current-build classification.
- **Mixed-channel downloads:** Reusing the current channel for archive fetches after
  reading a target manifest could combine unrelated assets. Bind manifest, archive,
  checksum, and staged identity to one resolved target channel.
- **Package-manager ownership:** A writable third-party binary could otherwise be
  mistaken for updater-managed state. Retain the existing embedded managed identity and
  installation safety checks; unsupported ownership still uses the installer or package
  manager.
- **Windows in-use replacement:** Channel switching reaches the existing complex handoff
  path. Do not add a second replacement implementation; retain native hosted validation
  as a completion gate.

## Completion State

Development. The CLI, target-channel routing, same-commit switching, safety boundaries,
tests, and documentation are implemented and validated. Managed Linux switches in both
directions are proven against the public rolling releases. Exact-revision hosted native
failure-boundary evidence remains required before transition to `done`.

## Development Validation Snapshot (2026-08-19)

- `task test:updater` passed 13 updater unit tests and 3 public CLI integration tests,
  including exact `development-latest` manifest/archive/checksum request paths,
  same-commit switching, bounded aliases, invalid input, output, and unmanaged
  rejection before network access.
- The post-review `task check` and independent `task test` runs passed formatting,
  Clippy with warnings denied, compilation, 744 library tests, 56 binary tests, every
  integration group, and doc tests. The explicitly gated paid live-provider test
  remained ignored.
- `task docs:check` passed all 5 documentation invariants, `task build` produced the
  locked optimized binary, and `git diff --check` passed.
- The first `task coverage` attempt exposed a transient failure in the unrelated Linux
  managed-process recovery test. That exact test passed in both normal full-suite runs
  and in the final exact-tree instrumented rerun. The final coverage gate passed at
  84.00 percent runtime line coverage (65,480/77,952); no updater test failed.
- The local source build is deliberately unmanaged, so a real binary replacement
  cannot be claimed from local evidence. Exact-revision hosted native runs and one real
  managed cross-channel round trip remain open.

## Review Reconciliation (2026-08-19)

- Spec compliance: the implementation preserves option-free behavior, resolves only
  typed compile-time channels, validates the selected manifest, binds the archive and
  checksum to that same target, requires staged embedded identity equality, and reuses
  the existing platform replacement protocol. The remaining unchecked criteria need
  hosted managed-install evidence rather than more local implementation.
- Code quality and security: the review found no updater-specific blocking issue. The
  download/verification slice was separated for deterministic target-channel path
  testing; unmanaged identity is still rejected before transport construction, and no
  configuration or arbitrary release URL was introduced.

## Development Release Evidence (2026-08-19)

- PR [#21](https://github.com/skills-yaml/nib/pull/21) passed exact-head Linux,
  Windows, and macOS CI in run
  [32254879268](https://github.com/skills-yaml/nib/actions/runs/32254879268) before
  merging to `development` as `c7ee849c669c9e93ec96281a602f928ae31a23cb`.
- Development release run
  [32256869402](https://github.com/skills-yaml/nib/actions/runs/32256869402) built,
  packaged, checksummed, and published the Linux, Windows, macOS Intel, and macOS Apple
  Silicon artifacts. The public nine-asset `development-latest` Release is a prerelease
  whose strict manifest names the exact merge commit.
- A freshly downloaded Linux archive passed its published SHA-256 checksum, reported
  `development` and the exact merge commit, and documented the bounded channel values.
  From that managed executable, `nib update --channel prod` reported an exact
  development-to-production switch to `79ea99d`; the replacement reported its embedded
  production identity and a subsequent option-free `nib update` was a successful no-op.
- The current production binary predates this option, so the reverse managed
  production-to-development switch cannot be exercised until the production release is
  approved and published. That direction and final startup-following evidence remain
  open.

## Production-to-Development Evidence (2026-08-23)

- A fresh isolated Linux download of `prod-latest` passed its published SHA-256
  checksum and reported the managed production identity
  `1abee6498de4ffbc195cca4f3d02f58697b25f04`.
- `nib update --channel development` reported the exact transition from production
  `1abee64` to development `5112a73`, then the replaced executable reported embedded
  development identity `5112a73c962b2d228f9b311a448b6101af477f01`.
- A native pseudo-terminal startup invocation continued to report that development
  identity, and the following option-free `nib update` was a successful development
  no-op. The qualification ran only in a fresh `/tmp` installation and did not mutate
  the source worktree.
- This closes the real managed reverse-direction and target-following criteria. The
  final exact implementation revision still requires the documented hosted native
  matrix and failure-boundary reconciliation before lifecycle completion.
