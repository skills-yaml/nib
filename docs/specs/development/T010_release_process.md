# T010: Release Process

**Status:** Development

## Problem Statement

nib needs a standardized, automated release process to distribute binaries to users across multiple platforms (Linux, macOS, Windows). This process should mirror the mature release pipeline established in the `skm` project to maintain consistency across the ecosystem.

## Goals

- Establish an automated CI/CD pipeline using GitHub Actions for releasing nib.
- Automate the build process for `x86_64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`.
- Distribute binaries into stable and prerelease channels based on the branch (`main` vs `development`).
- Automatically manage rolling tags (`prod-latest` and `development-latest`).
- Include SHA-256 checksums for all release artifacts.

## Scope

- Create a GitHub Actions workflow (`.github/workflows/release.yml`) adapted from `skm`.
- Ensure the build process utilizes `Taskfile.yml` (`task build`).
- Set up proper Github Release views and asset uploads.

## Design / Implementation Details (Copied from skm)

### Branches and Channels
- `main` branch pushes build to the `prod-latest` tag and publish as a standard Release.
- `development` branch pushes build to the `development-latest` tag and publish as a Pre-release.

### Workflow Pipeline
1. **Prepare Phase**: Determine the branch (`main` vs `development`) and set variables for channel (`prod` vs `development`), tag (`prod-latest` vs `development-latest`), and prerelease flag.
2. **Build Phase**:
   - Matrix strategy targeting:
     - Ubuntu (`x86_64-unknown-linux-musl`) -> `.tar.gz`
     - macOS Intel (`x86_64-apple-darwin`) -> `.tar.gz`
     - macOS Apple Silicon (`aarch64-apple-darwin`) -> `.tar.gz`
     - Windows (`x86_64-pc-windows-msvc`) -> `.zip`
   - Invokes `task build TARGET=<target>` (which in turn runs `cargo build --release --target <target>`).
   - Packages binaries into compressed archives along with a generated `.sha256` checksum.
   - Uploads these artifacts to the workflow space.
3. **Release Phase**:
   - Downloads all artifacts from the build matrix.
   - Forcibly moves the git tag (`prod-latest` or `development-latest`) to the current commit `GITHUB_SHA`.
   - Uses the `gh` CLI to create or edit the release, uploading all `.tar.gz`, `.zip`, and `.sha256` files.

## Exit Criteria

- `.github/workflows/release.yml` is committed and merged into `nib`.
- Pushing to `main` or `development` triggers a successful release build.
- Downloadable artifacts (binaries + checksums) are visible on the GitHub Releases page.

## Reopened Audit (2026-07-15)

Scope: prove the development channel in addition to the already verified production
channel, including prerelease metadata, rolling tag, four platform archives, and
matching checksum assets.

Affected areas: `.github/workflows/release.yml` and the repository's GitHub Actions
and Releases state.

Validation gates: a successful `channel=development` workflow dispatch and inspection
of the published `development-latest` prerelease assets.

## Validation Evidence (2026-07-15)

- GitHub Actions run `29395116732` completed successfully from a manual
  `channel=development` dispatch on `main` at commit `1afeed82`. That run validates
  the four-platform matrix, rolling prerelease tag, publication flow, and asset set.
- The `development-latest` release is a non-draft prerelease.
- The release contains Linux x86_64, macOS x86_64, macOS aarch64, and Windows
  x86_64 archives, each with a matching `.sha256` asset.
- The production `prod-latest` channel was separately verified during the same
  audit against its successful release workflow and complete asset set.

## Implementation Reconciliation (2026-07-15)

### Scope

Keep the verified four-platform release pipeline while ensuring every checksum file
contains the portable archive basename expected by the Unix and PowerShell installers.

### Acceptance Criteria

- [x] Production and development channels publish the required platform archives and matching checksum assets.
- [x] Unix and Windows manifests name the archive without a build-directory prefix.
- [x] Installer checksum mismatch paths fail closed before extraction or installation.
- [x] Overlapping runs are serialized per release channel and an older SHA cannot
  replace a newer rolling tag or asset set.
- [x] A late source advance or partial asset-publication failure cannot leave the rolling
  tag permanently paired with a prior release's assets; publication is staged and either
  committed coherently or rolled back with lease protection.
- [x] Local regressions cover source advance after tag preparation and failed asset
  publication/rollback ordering.
- [ ] The exact current workflow revision completes a development-channel dispatch
  and publishes all expected archives/checksums after it is committed.

### Affected Areas

`.github/workflows/release.yml`, `scripts/publish-release.sh`, `scripts/install.sh`,
`scripts/install.ps1`, and `tests/installers.rs`.

### Implementation Evidence

The successful published run above validates the matrix and GitHub Release mechanism.
The current workflow changes only Unix checksum-manifest path handling; the manifest
is now generated from inside `dist/`, matching the already portable Windows output.

### Validation Evidence

`tests/installers.rs::release_workflow_emits_portable_checksum_manifests` guards both
workflow manifest shapes and upload ordering. The installer integration tests verify
successful checksum handling and fail-closed mismatch behavior.

### Historical Validation Gates

This checked result describes the earlier checksum reconciliation snapshot. The final
transaction and remote gates below are authoritative for completion.

- [x] `task test` and `task check` with the current workflow regression test.

### Superseded Gap Assessment

An exact-current-SHA GitHub run can occur only after commit/publication. The current
checksum delta is covered locally and does not change the previously verified build
matrix or release mechanism, but the remote gate remains open rather than being
claimed against an older SHA.

The transaction remediation below supersedes the earlier local race description. Its
known GitHub API limitation is governed by the selected exclusive-writer contract; the
exact-current remote run remains open.

## Final Transaction Review Remediation (2026-07-15)

### Scope

Make the rolling release update a recoverable, identity-checked transaction across Git
refs and GitHub Release records, including process loss between steps. Tighten channel
provenance, artifact validation, and workflow permissions at the same boundary.

### Acceptance Criteria

- [x] Production publication is accepted only from `main`, and development publication
  only from `development` or an explicitly documented manual-dispatch source policy.
- [x] Every release mutation or deletion first proves the record still has the exact
  transaction tag owned by this channel; failed reads and observable external retags
  fail closed. The documented exclusive-writer contract removes simultaneous external
  mutation from the supported release topology.
- [x] Rollback and forward repair preserve the complete staged assets until a final
  coherent tag/release pairing is re-read and proven.
- [x] A branch or rolling-tag change during promotion prevents the transaction from
  setting its commit marker, with deterministic late-race coverage.
- [x] Durable per-channel staging and backup refs let a later run reconcile an abrupt
  predecessor before starting new publication; kill-window/rerun states are covered.
- [x] The staged release carries a versioned machine-readable transaction marker that
  binds the channel, candidate SHA, prior SHA, staged release ID, and prior release ID;
  recovery rejects missing, malformed, or mismatched ownership evidence.
- [x] The exact expected archive/checksum names, pairings, and checksum contents are
  validated locally; staged release names, uploaded state, and positive sizes are
  re-read before promotion.
- [x] Prepare/build jobs have read-only contents permission; write permission is scoped
  to publication, and third-party actions are pinned to immutable commits.
- [x] Local success, partial upload, source advance, ambiguous API success, failed
  detachment, failed old-release restoration, failed forward repair, and failed ref
  rollback paths all retain or establish a coherent recoverable state.
- [x] Rollback cleanup preserves the current staged marker until the backup ref is
  removed, and recovery classifies a coherent backup-only rollback terminal state
  before interpreting an older marker on the restored stable Release; a
  second-or-later publication kill/rerun regression covers this boundary.

### Affected Areas

`.github/workflows/release.yml`, `scripts/publish-release.sh`, `tests/installers.rs`,
`docs/tech/ci.md`, and release guidance in `docs/user/guide.md`.

### Durable Transaction State

Each channel reserves fixed staging and backup tags. The staged release body contains
a versioned marker recording the channel, candidate SHA, prior SHA (or `none`), staged
release ID, and prior release ID (or `none`). Recovery runs before validating a new
workflow SHA and treats every ref or API read as present, absent, or error. It mutates
records only by the recorded ID after re-reading the exact owned tag, uses exact Git
leases for ref changes, and retains every recovery artifact unless a complete forward
or rollback terminal state has been re-read successfully.

### Known GitHub API Limitation

The GitHub Releases update and delete endpoints address a record by numeric ID but do
not support conditional `PATCH` or `DELETE` preconditions. The implementation proves
the exact tag immediately before each mutation and re-reads the result, so failed reads
and retags visible before mutation fail closed. It cannot prevent an independent
`contents:write` actor from retagging the same release in the interval between that
proof and GitHub applying the ID-addressed mutation.

The project selected the exclusive-writer option: only the repository release workflow
may mutate rolling, staging, or backup Releases and tags, and its publication job is
serialized through the channel-specific protected environment. Personal tokens, GitHub
Apps, and other workflows must not hold equivalent publication authority while this
topology is enabled. An immutable per-transaction release plus Git-CAS channel manifest
remains the migration path if multiple writers become a requirement. GitHub documents that conditional
requests for unsafe REST methods are unsupported unless an endpoint explicitly opts in:
<https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api#use-conditional-requests-if-appropriate>.

### Validation Gates

`task installers:check`, the release transaction integration harness, `task test`,
`task check`, and the exact-current-SHA remote development-channel dispatch.

### Local Validation Evidence

The serial installer/release harness covers coherent success, partial assets, late
source movement before and after promotion, ambiguous API success, restoration and
forward-repair failures, process death followed by ordered recovery, a rolling tag
without a GitHub Release, malformed/mismatched markers, externally retagged prior
records, and GitHub/Git read-error fail-closed behavior. It also covers a
second-or-later rollback interrupted before exact-lease backup cleanup and the legacy
backup-only crash shape where the restored stable Release contains an older valid
transaction marker. Static workflow assertions cover branch provenance, permission
scope, and every pinned action commit.

### Remaining Gates

- [x] Adopt and document the exclusive-writer policy for rolling Releases, serialize
  the publication job through `release-prod` / `release-development`, and keep release
  mutation authority out of personal tokens, apps, and other workflows.
- [ ] Run the exact committed workflow revision successfully for the development channel
  and inspect its complete published artifact set.

## Remaining Implementation Plan

1. Commit the exact workflow revision, dispatch the development channel, inspect every
   published archive/checksum pair, and rerun the canonical gates.

## Current Risks

- Violating the exclusive-writer authority policy reintroduces a GitHub API race that
  the rolling Release endpoints cannot close with compare-and-swap.
- The exact current remote workflow revision has not run; local harness results do not
  substitute for that external evidence.

## CI Portability Remediation (2026-07-20)

### Problem

The canonical `task installers:check` gate used `rg` for four fixed-file assertions,
but the clean Ubuntu GitHub Actions runner does not install ripgrep. The release checks
therefore failed before Rust validation even though their repository and checksum
assertions require only a standard fixed-string search.

### Scope And Design

Use POSIX `grep` with fixed-string and quiet flags for the repository and checksum
assertions, retaining case-insensitive checksum matching for the PowerShell installer.
Do not add a workflow-only dependency for a gate that should remain portable when run
through Task.

### Non-Goals

- Changing installer behavior, release artifact contents, or checksum policy.
- Updating unrelated GitHub Actions or their runtime versions.

### Acceptance Criteria

- [x] `task installers:check` no longer invokes undeclared `rg` tooling.
- [x] The Unix and PowerShell installers are still checked for the canonical repository
  and checksum handling.
- [ ] The exact PR revision passes the hosted `Validate` job from a clean runner.

### Affected Areas

`Taskfile.yml` and the pull-request validation workflow execution.

### Validation Gates

`task installers:check`, `task check`, `task docs:check`, and the hosted PR `Validate`
job on the exact committed revision.

### Risks And Mitigations

Plain grep patterns could accidentally change matching semantics. Fixed-string `-F`
preserves literal matching, while `-i` remains limited to the PowerShell checksum check.

## Hosted Matrix Environment Remediation (2026-07-20)

### Scope And Design

Place macOS test and smoke fixtures under the runner's physical temporary directory.
The default macOS `/var/folders` path traverses the `/var` symlink and would make
security tests fail before reaching their deliberately injected link. Do not override
Windows TEMP/TMP: its default DOS-short path is valid product input and remains the
native canonical-alias regression.

### Acceptance Criteria

- [x] A macOS setup step appends `TMPDIR=$RUNNER_TEMP` to `GITHUB_ENV` so every later
  child process uses the physical runner temporary directory.
- [x] The macOS release smoke creates its fixture explicitly below `RUNNER_TEMP`.
- [x] Windows CI retains the runner's default temporary environment.
- [x] Native test fixtures keep an MCP error-producing child alive until its response is
  consumed and accept every bounded cleanup-deadline diagnostic emitted by the runtime.
- [ ] The exact PR revision passes the hosted macOS and Windows jobs.

### Affected Areas And Validation

`.github/workflows/ci.yml`, `docs/tech/ci.md`, the full native test suites, release builds,
and smoke commands. The final gate is a successful hosted matrix on the exact revision.

### Risk

This runner setup does not relax path validation. Ambient macOS product staging that
uses the platform default temporary path remains separate T006/FT-006 work and must not
be declared complete from this CI fixture change.

## Hosted Windows Workflow Fixture Portability (2026-07-21)

### Scope And Design

Make static release-workflow regressions independent of Git checkout line-ending policy.
Canonicalize repository fixture text from CRLF to LF at the shared test read boundary,
then retain the exact source assertions for action pins, permission scope, ordering, and
embedded shell commands. Do not change the release workflow or publication behavior.

### Acceptance Criteria

- [x] Release-workflow source assertions consume the same canonical LF representation on
  Unix and Windows checkouts.
- [x] A deterministic regression covers CRLF canonicalization without weakening any
  workflow-content or transaction-order assertion.
- [ ] The exact PR revision passes the full hosted Windows job and PR matrix.

### Affected Areas And Validation

`tests/installers.rs`, release-workflow validation evidence, `task test`, `task check`,
Windows-target `task check:all-targets`, and the exact-revision hosted matrix.

### Reproduction Evidence

Hosted Windows run `29797222053` passed all 543 library tests, all four durable-worker
tests, and the gateway timing regression before two installer tests read a CRLF checkout
of `.github/workflows/release.yml` and failed exact substrings containing LF. Four later
LF-sensitive assertions were masked by those first failures; the shared canonicalization
covers all of them. Canonicalizing fixture text at read time preserves the intended
source-level checks and leaves production files unchanged.
