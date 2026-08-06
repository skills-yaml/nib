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

## FT-018 Release Manifest Extension (2026-08-01)

### Scope And Design

Generate a strict `nib-release.json` from the already validated archives and portable
checksum files. The manifest binds repository, channel, rolling tag, package version,
candidate commit, and the size/SHA-256 identity of each platform archive. It is the
ninth candidate asset and follows the existing staging, promotion, recovery, and
rollback transaction.

The first manifest-producing transaction accepts the historical eight-asset rolling
Release as a coherent predecessor. Every new candidate requires the manifest; legacy
acceptance is restricted to prior/backup state and cannot make a new incomplete stage
coherent.

### Acceptance Criteria

- [x] Local generation emits strict JSON with the exact candidate and four archive
  identities.
- [x] Staged candidate validation requires the manifest plus all four archives and four
  checksum files.
- [x] Rollback and forward recovery keep the manifest coherent with the candidate while
  accepting a legacy predecessor.
- [x] The 23-test local installer/release harness passes success, failure, process-loss,
  stale-source, retag, read-error, rollback, and forward-repair paths.
- [ ] An exact committed development-channel run publishes and validates the nine-asset
  release before FT-018 production rollout.

### Affected Areas And Validation Gates

`scripts/publish-release.sh`, `tests/installers.rs`, `docs/tech/ci.md`, FT-018, and the
hosted development release. Local gates are `task installers:check` and
`task test:installers`; the existing exact-current remote gate remains authoritative.

### Risk

If legacy assets were accepted for a newly staged candidate, a missing manifest could
be promoted. Candidate and predecessor validation are therefore separate: only the
predecessor may use the legacy eight-asset set.

## Workflow-Permission CI Remediation (2026-08-02)

### Problem

Production run `30707796559` built all four platform artifacts but failed before
staging because the workflow `GITHUB_TOKEN` attempted to create the backup tag at the
older release SHA. That predecessor contains a different `.github/workflows/` tree,
and GitHub refuses a GitHub App token without the separate Workflows permission. The
Actions `GITHUB_TOKEN` cannot be granted that permission.

### Scope And Design

Retain the rollback-capable staging/backup transaction when the candidate and
predecessor have the same workflow tree. When they differ, select a forward-only mode
that never creates a ref or retags a Release at the predecessor SHA:

1. Create and fully validate the candidate draft and nine-asset set.
2. Record `transaction_mode=forward-only` and transition the durable marker from
   `staged` to `forward` before removing public predecessor state.
3. Delete the prior Release by its revalidated ID, move the rolling tag to the current
   source SHA through the Git refs API, and promote the staged Release.
4. Before the phase transition, recovery removes the stage and preserves the prior
   channel. After it, recovery can only converge forward to the already validated
   candidate.

The mode uses the existing serialized environment and exclusive-writer policy. It does
not add a PAT, another GitHub App, or a second publisher.

### Acceptance Criteria

- [x] A workflow-tree change selects forward-only publication and creates no backup
  ref or backup Release.
- [x] The candidate is complete and marker-owned before the forward boundary is
  durable.
- [x] Process loss immediately after prior-Release deletion is recovered forward on a
  rerun, leaving one coherent rolling ref/Release pair.
- [x] Ordinary releases retain the existing rollback transaction and its fault matrix.
- [x] `task installers:check` and all 25 installer/release transaction tests pass.
- [ ] The exact committed production run passes and publishes the nine-asset Release.

### Affected Areas And Risk

`scripts/publish-release.sh`, `tests/installers.rs`, and `docs/tech/ci.md` are affected.
The forward boundary intentionally gives up rollback only after the candidate is fully
validated; failures after that point may leave a failed workflow run but must leave or
recover a coherent candidate channel. A development candidate whose workflow tree
still differs from the repository default branch remains an external GitHub token
constraint and is not claimed by the production-path evidence.

## Hosted macOS Bash Portability Remediation (2026-08-06)

### Reproduction And Scope

PR run `31083581973` reached the installer transaction suite on macOS after the provider
fixtures passed, then exposed two Bash 3.2 incompatibilities that Linux's Bash 5 did not:
manifest hash normalization used `${var,,}`, and the new mock Git-refs DELETE path
expanded an empty array under `set -u`. Reproduce the runner shell locally with Bash
3.2.57 and run the complete installer test binary.

### Acceptance Criteria And Gates

- [x] Release-manifest commit and digest normalization use portable `tr` conversion.
- [x] The fake Git-refs API avoids empty-array expansion for DELETE requests.
- [x] All 25 installer/release tests pass under Bash 3.2.57 and the default local shell.
- [ ] The exact committed PR revision passes hosted macOS, Windows, and Validate jobs.

Affected areas are `scripts/publish-release.sh`, `tests/installers.rs`, and
`docs/tech/ci.md`. Validation gates are the Bash 3.2 installer suite,
`task test:installers`, `task check`, `task docs:check`, and the exact-revision hosted
matrix.

## Updater Rollout Qualification (2026-08-06)

### Scope

Preserve T010's exclusive release writer while adding a separate manual, read-only
workflow that qualifies FT-018 across two published development revisions. The
qualification downloads the first successful Release Artifacts run's native archives
and exercises the second public `development-latest` manifest; it does not create,
retag, edit, or delete any Git ref or Release.

Because GitHub exposes manual dispatch only for workflows present on the default branch,
the qualification revision first lands on `main` while `release-prod` has a required
reviewer and its exact production publication job remains unapproved. The same main SHA
is then moved to `development`, published there, and qualified. Only the held production
run for that already-qualified SHA may be approved. The workflow validates the exact
bootstrap, candidate, and held-production run IDs, their workflow path and workflow ID,
bootstrap ancestry, chronological and run-ID order, and absence of an intervening
successful development publication. A final post-matrix job revalidates that the exact
production deployment is still pending on `release-prod` and no production ref moved.

Hosted qualification exposed that GitHub can asynchronously rewrite a private draft to
an `untagged-*` placeholder even when the publisher created the reserved staging ref at
the exact candidate SHA first and supplied `--verify-tag`. The publisher therefore
follows the unique private draft by immutable Release ID, exact transaction marker,
target commit, asset set, and channel while accepting only GitHub's bounded
`untagged-*` form as an alternate staged state. A rerun recovers or removes that exact
marked draft even if the staging ref was already cleaned, including rollback-mode
states after the prior release was backed up or the rolling ref reached the candidate;
it may finalize a pending immutable-ID marker only after proving the exact private
draft, while removal or public mutation additionally requires coherent rolling and
backup state. Ambiguous or mismatched drafts and failed discovery reads fail closed. A
stage-only crash artifact remains removable only when the ref still names the same
`GITHUB_SHA`, the stable release remains coherent, and any backup ref still names that
stable rolling SHA.

### Acceptance Criteria And Gates

- [ ] Two distinct exact-revision development publications complete with nine coherent
  assets before production promotion.
- [ ] The qualification workflow runs only from `development`, has only `actions: read`
  and `contents: read`, and pins every third-party action to a reviewed commit.
- [ ] All four release platforms prove the first binary notices and self-replaces with
  the second exact development build, then reports a byte-preserving current no-op.
- [ ] The exact candidate SHA is present on both `main` and `development`, its production
  publication remains held by `release-prod`, and that same run is approved only after
  canonical local, hosted CI, and four-platform update qualification gates pass.
- [ ] Documentation-only and agent-memory-only pushes do not republish unchanged
  binaries; post-rollout evidence can be reconciled without moving a qualified channel.
- [ ] Draft upload requires an explicitly created, exact-SHA staging ref; publication
  and rerun recovery preserve immutable-ID ownership if GitHub rewrites that private
  draft to `untagged-*`, without changing the prior rolling release.

Affected areas are `.github/workflows/release.yml`, `scripts/publish-release.sh`,
`.github/workflows/release-update-qualification.yml`, the two native qualification
scripts, `Taskfile.yml`, `tests/installers.rs`, and release CI docs.

## Headless Windows Terminal Qualification Remediation (2026-08-06)

### Reproduction And Scope

Qualification run `31113952334` passed Linux and both macOS runners, then failed on
Windows before starting nib because Git for Windows' `winpty.exe` requires its own
standard input to already be a terminal. GitHub Actions launches PowerShell with pipe
handles, so `winpty.exe` reports `stdin is not a tty` and cannot be used to create the
first terminal in that environment.

Replace that adapter with a repository-owned host for the native Windows ConPTY API.
The host must create and drain a pseudoconsole without interactive parent handles,
capture the child's combined console stream, preserve its exit code, enforce a timeout,
and release every pipe, process, attribute-list, and pseudoconsole handle. Run that host
behind a separately killable process boundary so a stuck operating-system close cannot
make qualification unbounded; timeout cleanup must terminate that complete process tree
and leave no console descendant. The normal hosted Windows CI job must prove that a
child launched through the host observes an interactive standard error handle before
release qualification relies on it.

Hosted Windows Server 2025 also duplicates the Actions runner's redirected standard
handles into a process attached directly to ConPTY when its startup handles remain
null. Attempts to repair those handles through an intermediate terminal root retained
the redirected error handle both with implicit console duplication and with an explicit
restricted handle list. The host must instead launch the requested process directly
with `STARTF_USESTDHANDLES`, all three standard-handle fields set to
`INVALID_HANDLE_VALUE`, handle inheritance disabled, and the pseudoconsole process
attribute present. This sentinel contract prevents redirected parent pipes from being
copied while allowing ConPTY to install the requested process's console handles. The
direct launch stays inside the separately killable ConPTY process tree and must preserve
the same output, exit-code, timeout, and descendant-cleanup contracts.

### Acceptance Criteria And Gates

- [ ] The Windows helper launches from headless PowerShell without downloading a
  runtime dependency and proves the child sees interactive console output.
- [ ] Windows qualification captures the bootstrap binary's candidate notice through
  the helper, then still proves exact replacement and a byte-preserving current no-op.
- [x] Deterministic repository tests bind the workflow, Task target, helper, and native
  qualification script together and reject a return to `winpty.exe`.
- [x] `task test:installers`, `task docs:check`, and `task check` pass locally.
- [ ] The exact committed revision passes hosted Validate, macOS, and Windows jobs, and
  a new read-only four-platform release-update qualification run passes before the
  exact held production deployment is approved.

Affected areas are `scripts/windows-pseudoterminal.cs`, its bounded PowerShell host and
invocation scripts, the Windows qualification and smoke-test scripts, `Taskfile.yml`,
`.github/workflows/ci.yml`, `tests/installers.rs`, and release/task CI docs.
