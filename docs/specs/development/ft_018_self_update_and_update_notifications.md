# FT-018: Self-Update Command and Update Availability Notices

**Status:** Development

## Summary

Add an explicit `nib update` command that safely replaces an installed official nib
binary with the current release from its existing rolling channel. When that channel
already points at the installed build, the command reports that there is nothing to
update and makes no filesystem changes.

Every ordinary user-facing nib launch also performs a bounded, best-effort availability
check. If the selected channel contains a different build, nib prints a concise notice
that directs the user to `nib update`; startup checks never install software.

## Decision

- Official release builds update only within their embedded `prod` or `development`
  channel.
- Build identity is the exact embedded commit SHA, not only the Cargo package version,
  because nib currently publishes mutable `prod-latest` and `development-latest`
  releases.
- Each rolling Release publishes a bounded `nib-release.json` manifest containing its
  channel, tag, commit, package version, and exact platform artifact metadata.
- `nib update` performs a foreground, integrity-checked update. Automatic startup checks
  only read the manifest and notify.
- Local, unknown, source-built, or otherwise unmanaged builds are not overwritten. They
  receive explicit reinstall guidance from `nib update` and do not emit automatic update
  notices.
- The repository release workflow remains the exclusive rolling-release writer defined
  by [T010](../development/T010_release_process.md).

## Problem

nib users must currently know that updating means rerunning a platform installer. The
CLI contains an unused updater stub, but it points at a placeholder repository, invokes
Git to compare a tag, and only prints installer guidance. There is no shipped `update`
subcommand and no routine notification that a newer channel build is available.

Cargo version `0.1.0` is also insufficient as the sole comparison key while the product
uses rolling channel tags. Two different release builds can have the same package
version, so availability must be bound to the release commit and channel.

## Goals

- Provide `nib update` on Linux, macOS, and Windows for official release binaries.
- Keep an already-current invocation a successful, observable no-op.
- Preserve the installed binary on download, validation, extraction, smoke, or
  replacement failure.
- Reuse the existing release channels, artifacts, checksum policy, and exclusive-writer
  transaction instead of creating a second distribution system.
- Check for channel updates during every eligible user-facing launch and notify only
  when an update is available.
- Keep startup checks bounded, non-mutating, quiet on network failure, and isolated from
  machine-readable protocols.
- Make all network, archive, path, concurrency, and replacement behavior deterministic
  enough for local and native-platform regression tests.

## Non-Goals

- Installing updates automatically without an explicit `nib update` command.
- Switching an installed binary between production and development channels.
- Updating source builds, package-manager installations, or arbitrary forks.
- Introducing semantic-version ordering or immutable versioned releases in this feature.
- Adding a background update daemon, scheduled task, telemetry, or a new global config
  store.
- Signing release artifacts. This feature retains the existing GitHub HTTPS plus
  SHA-256 trust boundary; artifact signing requires a separate supply-chain decision.
- Changing the release workflow's exclusive-writer policy or its recovery model.

## Terminology And Invariants

- **Managed build:** a binary with embedded channel `prod` or `development`, a valid
  lowercase 40-hex commit SHA, and an executable path that the updater can replace
  without privilege escalation.
- **Current channel build:** the build described by `nib-release.json` at the installed
  build's rolling tag.
- **Update available:** the validated manifest commit differs from the installed
  embedded commit. Because tags are rolling, user-facing copy says "channel update"
  rather than claiming semantic-version ordering.
- **Already current:** the manifest channel and commit exactly equal the installed
  channel and commit.
- Update checking and installation are product-maintenance operations. They do not
  create a project session, mutate the workload model, or pass through `ToolExecutor`.
- A startup check never mutates the executable, release state, project state, profile
  state, or session state.

## Proposed Design

### Release Manifest Contract

The release workflow adds `nib-release.json` to both rolling releases. The manifest is
UTF-8 JSON, has a maximum accepted size of 64 KiB, rejects unknown fields, and contains:

```json
{
  "schema_version": 1,
  "repository": "skills-yaml/nib",
  "channel": "prod",
  "tag": "prod-latest",
  "version": "0.1.0",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "assets": {
    "nib-linux-x86_64.tar.gz": {
      "sha256": "<64 lowercase hex characters>",
      "size": 1
    }
  }
}
```

The real manifest contains all four supported archives. Publication validates that:

- repository, channel, tag, package version, and candidate commit match the release
  transaction;
- the asset map contains exactly the supported platform archives;
- every size is positive and every digest matches both the archive and its existing
  `.sha256` asset;
- the manifest, four archives, and four checksum files are uploaded to the staged
  Release before promotion; and
- recovery preserves or rolls back the manifest with the same atomic release unit as
  the other assets.

Existing installers remain compatible because adding the manifest does not change
their archive or checksum URLs. The manifest URL is:

```text
https://github.com/skills-yaml/nib/releases/download/<rolling-tag>/nib-release.json
```

The client accepts HTTPS redirects only to a bounded allowlist of GitHub-controlled
release hosts. It does not accept a repository or manifest URL from project config,
environment variables, or release content.

### `nib update` Command

`nib update` follows this flow:

1. Read and validate the embedded build channel, commit, package version, target OS,
   architecture, and current executable path.
2. Reject unmanaged builds before network or filesystem mutation, with guidance to
   rerun the official installer.
3. Acquire an exclusive, installation-target-scoped update lock. A concurrent updater
   exits nonzero with `another nib update is already in progress`.
4. Fetch and strictly validate the channel manifest with bounded redirects, response
   bytes, connect timeout, total timeout, and no unbounded retry.
5. If the manifest commit equals the embedded commit, print the current version,
   channel, and short commit plus `nib is already up to date`; exit zero without
   downloading an archive or touching the executable.
6. Select exactly one supported archive from the compiled target OS and architecture.
7. Download that archive and its `.sha256` asset into a fresh private temporary
   directory under explicit byte and time limits.
8. Require the manifest digest, checksum asset digest, and downloaded archive digest to
   match exactly before extraction.
9. Extract without following links or accepting absolute paths, `..` traversal, device
   entries, unexpected files, duplicate binary entries, or an oversized expanded
   binary.
10. Execute the staged binary with `version` under `NIB_NO_UPDATE_CHECK=1` and require
    its embedded repository contract, channel, version, and full commit to match the
    manifest.
11. Revalidate the locked target path and file identity immediately before replacement.
    A replaced path, changed identity, symlink/reparse-point ambiguity, or lost lock
    fails closed.
12. Commit the new executable through a same-directory, same-filesystem replacement
    protocol. Unix uses an atomic rename over the old executable after syncing the
    staged file and parent directory. Windows uses a native, tested self-replacement
    protocol that succeeds only after the target path names the verified new binary;
    cleanup of the old in-use image may be deferred but must be bounded and recoverable.
13. Print the previous and installed version/channel/short-commit identities and exit
    zero.

The updater never invokes `sudo`, changes `PATH`, mutates Git tags or Releases, or
silently falls back to executing a downloaded installer script. A non-writable or
unsupported installation returns a nonzero error with the exact manual installer
command for the embedded channel.

### Output Contract

Successful no-op:

```text
nib is already up to date: 0.1.0 (prod, 0123456)
```

Successful replacement:

```text
Updated nib: 0.1.0 (prod, 0123456) -> 0.1.0 (prod, 89abcde)
```

Unmanaged build:

```text
This nib build is not self-update managed (channel: local). Reinstall from an official prod or development release.
```

Normal results go to stdout. Actionable failures go to stderr and return nonzero. Error
messages identify the failing stage without exposing temporary paths or unbounded HTTP
response bodies.

### Startup Availability Check

After command parsing, every ordinary user-facing process launch invokes the same
manifest comparison code with a one-second maximum total budget and no retry. The
check:

- runs for visible commands such as the default UI, `chat`, `run`, `tui`, `auth`,
  `context`, `config`, `doctor`, `skill`, `mcp`, `task`, and `version`;
- is replaced by the foreground update flow for `nib update`;
- is skipped for Clap's early `--help`/`--version` exits, `mcp-server`, stdio relay,
  task workers, supervisor/worker commands, and test fixtures;
- is skipped for unmanaged builds and when `NIB_NO_UPDATE_CHECK=1` is set;
- downloads only the bounded manifest and never an archive;
- emits at most one notice per process, to stderr, only when stderr is an interactive
  terminal and a different validated commit is available;
- remains silent when the current build matches, the network is offline, the request
  times out, the manifest is missing, or validation fails; and
- never changes the invoked command's exit status.

The notice is concise and includes the channel, package version, and short commit:

```text
[nib] Channel update available: 0.1.0 (prod, 89abcde). Run `nib update`.
```

Startup failures may be recorded through debug logging, but they are not written to
session history and do not create durable update state. The environment opt-out and
the fact that startup checks contact GitHub are documented for offline and privacy-
sensitive environments.

### Shared Components

The explicit command and startup notice share typed code for:

- channel-to-tag mapping;
- embedded and remote build identity validation;
- platform-to-asset selection;
- manifest fetching, size limits, timeouts, and redirect policy; and
- exact `Current`, `Available`, `Unmanaged`, and `Unavailable` outcomes.

Only the explicit command can call archive download, verification, extraction, locking,
or replacement code. This separation prevents a startup-check call site from gaining
mutation capability accidentally.

## Security And Reliability Requirements

- Validate all release metadata before constructing download paths.
- Keep the official repository and rolling tags compile-time controlled.
- Reuse `rustls`; do not add a native TLS dependency or shell out to Git/curl.
- Bound manifest, checksum, archive, expanded-binary, redirect, response-time, and
  extraction-entry counts.
- Require three-way archive digest agreement: manifest, checksum asset, and downloaded
  bytes.
- Smoke the staged executable and verify its embedded identity before replacement.
- Reuse the project's filesystem identity and no-link primitives where applicable.
- Serialize mutation per installed target and recheck identity immediately before the
  commit point.
- Preserve the prior executable on every pre-commit error and across injected process
  failures. Post-commit recovery must converge on one complete verified executable.
- Never let update-check output enter MCP stdout, JSON output, model context, or session
  audit records.

## Affected Areas

- `src/main.rs` — public subcommand and eligible-startup dispatch.
- `src/updater.rs` — replace the placeholder implementation with typed check, download,
  verification, and replacement logic.
- `src/version.rs` and `build.rs` — expose and validate a consistent full build identity.
- `src/fs_security.rs` — reuse or extend exact path/file identity checks if required.
- `Cargo.toml` / `Cargo.lock` — bounded archive or platform replacement support, if the
  standard library and existing dependencies are insufficient.
- `.github/workflows/release.yml` and `scripts/publish-release.sh` — generate, validate,
  stage, recover, and publish `nib-release.json` within T010's transaction.
- `tests/installers.rs` and CLI/integration tests — release contract, failure injection,
  replacement, output, and startup-check coverage.
- `README.md`, `docs/user/guide.md`, `docs/tech/ci.md`, and
  `docs/tech/project_structure.md` — command, notification, opt-out, release manifest,
  and updater architecture documentation.
- [T010](../development/T010_release_process.md) — coordinated release evidence and
  exact-current remote rollout gate; its lifecycle state remains independently owned.

The agent loop, tool permission model, sessions, durable tasks, MCP protocol, and
workload persistence are not modified.

## Alternatives Considered

- **Keep rerunning installers:** safe but rejected as the primary experience because it
  provides neither a discoverable command nor availability notices.
- **Use `git ls-remote`:** rejected because update discovery should not spawn Git,
  inherit repository config, or depend on local Git transport behavior.
- **Use the unauthenticated GitHub REST API on every run:** rejected because API quota
  and response variability are unnecessary for a fixed rolling release asset.
- **Compare only `CARGO_PKG_VERSION`:** rejected because rolling releases can contain
  distinct commits with the same package version.
- **Download the full archive to check availability:** rejected because startup checks
  require only small bounded metadata.
- **Automatically update during startup:** rejected because executable mutation must be
  an explicit user action with visible errors.
- **Execute a freshly downloaded installer script:** rejected because the updater can
  verify and replace the known platform artifact directly without executing mutable
  remote shell code.

## Rollout Plan

1. Extend T010's staged release transaction and installer regressions to generate and
   verify `nib-release.json` without changing existing archive/checksum URLs.
2. Implement typed manifest checking and `nib update` behind the new public command;
   validate no-op and replacement flows against a local fake release server.
3. Add the bounded startup check only after the explicit checker has deterministic
   timeout, output-isolation, and unmanaged-build behavior.
4. Publish and inspect a development-channel release containing the manifest and
   updater. Use the existing installer for this bootstrap release.
5. From that installed development build, publish a second development release and
   prove notification plus end-to-end self-update on Linux, macOS Intel, macOS Apple
   Silicon, and Windows x86_64.
6. Promote the same contract to production only after all local and hosted gates pass.

The first updater-capable release still requires the existing installer. Self-update is
available from that release onward. A missing manifest makes startup checking silent and
causes `nib update` to fail with installer guidance; it never guesses from partial data.

## Implementation Plan

1. Define the manifest schema and pure build/channel/platform comparison types.
2. Extend release generation, staged-asset validation, and recovery tests.
3. Implement bounded manifest transport and deterministic result classification.
4. Implement archive verification, safe extraction, staged-binary identity smoke, and
   per-target locking.
5. Implement and natively test Unix and Windows replacement protocols.
6. Wire `nib update`, then wire the read-only startup check through an explicit command
   eligibility policy.
7. Update user, technical, installer, and CLI documentation.
8. Run local gates, hosted platform gates, and two consecutive development releases
   before production rollout.

## Validation Gates

- Pure tests cover channel/tag mapping, equal and different commits, unmanaged builds,
  malformed identities, unsupported targets, unknown fields, manifest byte limits, and
  exact asset selection.
- HTTP fixture tests cover success, offline/timeout, redirects, redirect rejection,
  truncated/oversized responses, malformed JSON, wrong repository/channel/tag/commit,
  and retry bounds.
- Update integration tests cover current no-op, successful replacement, checksum and
  manifest mismatch, unsafe archives, staged-binary identity mismatch, non-writable
  targets, replaced target identities, concurrent invocations, and injected failures at
  every pre- and post-commit boundary.
- CLI tests assert stable stdout, stderr, and exit status for current, updated,
  unmanaged, unavailable, and failed outcomes.
- Startup tests prove every eligible command invokes the checker, excluded protocol and
  worker commands do not, only interactive stderr receives notices, timeouts stay within
  budget, and check failures never change command results.
- Release transaction tests prove the manifest is part of the complete staged asset set
  and survives forward recovery or rollback coherently with all archives/checksums.
- Native Linux, macOS Intel, macOS Apple Silicon, and Windows tests replace a real copied
  executable and verify the next invocation reports the manifest identity.
- `task installers:check`, `task docs:check`, `task test`, `task check`, `task coverage`,
  and `task build` pass.
- Two consecutive exact-revision development release runs prove bootstrap installation,
  update notification, verified self-replacement, and complete published assets before
  production enablement.

## Acceptance Criteria

- [ ] `nib update` is visible in CLI help on supported release targets.
- [ ] When the installed commit equals the validated channel manifest, `nib update`
  exits zero, prints `nib is already up to date`, downloads no archive, and leaves the
  executable untouched.
- [ ] When a different valid channel build exists, `nib update` downloads only the
  correct platform archive, validates the manifest/checksum/archive digests, smokes the
  staged identity, replaces the executable safely, and reports both identities.
- [ ] Local, unknown, unsupported, non-writable, and ambiguous installations fail
  without mutation and provide actionable official-installer guidance.
- [ ] Download, checksum, extraction, smoke, race, or replacement failure cannot leave
  the target path missing, partially written, or pointing at an unverified binary.
- [ ] Concurrent update attempts serialize and cannot overwrite a newer verified
  installation with stale downloaded state.
- [ ] Every eligible user-facing launch performs a bounded availability check and emits
  at most one interactive stderr notice when a different validated commit exists.
- [ ] Startup checks never install, never alter exit status, stay silent for current or
  unavailable state, honor `NIB_NO_UPDATE_CHECK=1`, and cannot contaminate MCP or other
  machine-readable output.
- [ ] The rolling release transaction publishes and validates `nib-release.json`
  coherently with the existing four archives and four checksum assets.
- [ ] User and technical docs describe the command, rolling-channel identity, automatic
  check, opt-out, failure behavior, bootstrap limitation, and manual recovery path.
- [ ] All local, hosted cross-platform, release, documentation, coverage, and consecutive
  development self-update gates pass on exact committed revisions.

## Risks And Tradeoffs

- A remote request on ordinary starts adds latency and reveals a GitHub release check.
  The request is manifest-only, bounded to one second, silent on failure, and can be
  disabled with `NIB_NO_UPDATE_CHECK=1`.
- Checksums obtained from the same GitHub release do not protect against compromise of
  the repository's release authority. This matches the current installer boundary but
  does not replace future artifact signing.
- Rolling tags can intentionally move backward during recovery or operator rollback.
  Exact identity comparison remains truthful, while UI copy avoids claiming semantic
  precedence.
- Windows in-use executable semantics make replacement more complex than Unix rename.
  Production acceptance requires a native hosted end-to-end replacement, not only
  cross-compilation or mocked filesystem tests.
- Extending the exact release asset set changes T010's recovery harness. The manifest
  must enter the existing transaction atomically; a parallel publisher is prohibited.
- An executable installed through an unsupported package manager could be writable but
  should not be overwritten. The embedded managed-channel requirement prevents local
  builds from being mistaken for official updater-owned installations.

## Dependencies

- T010 remains the authoritative release publication and recovery contract. FT-018 can
  be developed locally while T010 is in development, but remote rollout cannot complete
  until the exact manifest-producing release workflow is green.
- Existing release build metadata must continue embedding the exact channel and commit.
- The four current platform archives and `.sha256` assets remain the installation unit.

## Open Questions

None are blocking for development. Package-manager ownership detection, signed
artifacts, channel switching, configurable persistent check cadence, and immutable
versioned releases require separate follow-up decisions if they enter scope.

## Implementation Reconciliation (2026-08-01)

### Implemented Scope

- `nib update` is a public command and local/unmanaged builds fail before network I/O.
- Official builds use their embedded channel and exact commit identity.
- Strict manifest parsing, bounded GitHub release transport, allowlisted redirects,
  target selection, checksum parsing, three-way SHA-256 agreement, safe tar/zip
  extraction, staged identity smoke, target locking, identity recheck, and Unix/Windows
  replacement paths are implemented in `src/updater.rs`.
- Ordinary visible commands invoke a one-second best-effort check. Update, MCP stdio,
  relay, task worker, and subagent worker/supervisor paths are excluded; notices require
  an interactive stderr and `NIB_NO_UPDATE_CHECK=1` disables checks.
- The release transaction generates `nib-release.json`, uploads it as the ninth candidate
  asset, and preserves legacy eight-asset releases as valid predecessors during rollout.
- README, user, architecture, CI, structure, Task, and lifecycle documentation describe
  the delivered local contract.

### Local Evidence

- `task test:updater`: 7 updater unit tests and 2 CLI integration tests passed. The
  localhost HTTP fixture requires a host that permits loopback binding.
- `task test:installers`: all 23 installer and release-transaction tests passed,
  including failure/recovery paths with the manifest candidate and legacy predecessor.
- Host `task check:all-targets` passed.
- Windows MSVC `task check:all-targets TARGET=x86_64-pc-windows-msvc` passed; warnings
  are pre-existing cross-target dead-code warnings outside FT-018.
- macOS cross-check reaches `ring` but cannot compile on this Linux host because an
  Apple compiler/SDK is unavailable; this is an environment gate, not updater evidence.

### Remaining Gates

- `task check` currently stops on four Clippy findings in the overlapping T021/T022
  provider work. A direct repository-wide `task test` reaches 681 passing library tests
  before eight provider-continuation failures in the same work. FT-018's focused gates
  are green, but the canonical repository-wide gates are not yet green.
- Native macOS and Windows executable replacement remains unexecuted.
- A manifest-producing development release and a second release that exercises actual
  notification and self-replacement have not run. The spec remains in Development.
