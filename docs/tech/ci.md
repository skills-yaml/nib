# CI for nib (Rust CLI)

Follows skm project structure.

## Pipeline
- Use `task check`, `task test`, `task coverage`, `task build`, and the Linux
  `task smoke:managed-process` release-binary owner-loss gate.
- Rust toolchain via dtolnay/rust-toolchain.
- Task via arduino/setup-task.
- Install bwrap on Linux and require the PID-namespace supervisor regressions.
- Keep broad bwrap/network diagnostics separate from the exact managed-process backend
  probe so restricted network namespaces do not suppress a usable PID supervisor.
- Run `task check:all-targets` and `task test` plus native backend and
  production-rejection tests on Windows and
  macOS in addition to the Linux validation/coverage and production supervisor smoke.
  Production delegation remains Linux-only until a separate spec proves protected
  cleanup authority on those platforms.
- Export macOS `TMPDIR` from the physical `RUNNER_TEMP` root through `GITHUB_ENV` because the platform's
  default `/var` temporary path traverses a symlink. Keep the Windows runner's default
  DOS-short temporary path so canonical alias handling remains a native regression gate.
- Cross-platform release builds for linux/macos/windows.
- Channels: prod (main), development.
- Workflow actions are pinned to reviewed commits. Repository contents are read-only
  except for the publication job, which receives `contents: write`.
- Installers in scripts/ consume the release artifacts.

See .github/workflows/ci.yml and release.yml (modeled directly on skm).

## Taskfile
See root Taskfile.yml for Rust check, test, coverage, documentation, installer, build,
and managed-process smoke tasks.

## Install & Update

### End-user Installation

Use the platform-specific installers from the release artifacts:

**Linux / macOS**
```bash
# Stable releases
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh

# Development channel
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib NIB_CHANNEL=development sh
```

**Windows (PowerShell)**
```powershell
$Repo = "skills-yaml/nib"
& ([scriptblock]::Create((irm "https://raw.githubusercontent.com/$Repo/main/scripts/install.ps1"))) -Channel prod -Repo $Repo -AddToPath
```

Installers:
- Download the correct asset (`nib-linux-x86_64.tar.gz`, `nib-macos-aarch64.tar.gz`, etc.)
- Verify the matching SHA-256 asset before extraction
- Install to `~/.local/bin` (or `%USERPROFILE%\.local\bin` on Windows)
- On Unix, print a PATH hint if needed; on Windows, update the user PATH when
  `-AddToPath` is passed

See:
- `scripts/install.sh`
- `scripts/install.ps1`
- `scripts/first-time-setup.sh` (runs `nib auth` after install)

### Development / Building

```bash
task build          # Release binary
task dev            # check + test + build + --help
./target/release/nib chat
```

The release workflow (`.github/workflows/release.yml`) builds for multiple targets and
publishes GitHub Releases through `prod-latest` / `development-latest`. Production is
accepted only from `main`; development is accepted only from `development`. Runs are
serialized per channel.

Publication normally uses fixed per-channel staging and backup refs plus a versioned
marker in the staged Release body. When `.github/workflows/` differs between the
predecessor and candidate, the workflow-scoped `GITHUB_TOKEN` cannot create a backup
ref at the predecessor because GitHub does not grant that token the separate Workflows
permission. That case uses a marked forward-only transaction: validate the complete
candidate draft first, durably record the forward boundary, delete the prior Release,
move the rolling ref to the source-branch candidate through the Git refs API, and
promote the candidate. A crash before the boundary rolls back the stage; a crash after
it converges forward on the next run. No personal token or parallel publisher is added.

Both modes verify the four exact archives, their four portable checksum manifests,
uploaded asset state, positive asset sizes, source-branch identity, and rolling-tag
identity before promotion. A later run reconciles an interrupted transaction before it
considers its own SHA. Recovery preserves candidate artifacts until a complete forward
or rollback state has been re-read.

The publisher also generates `nib-release.json` after validating the archives. The
strict manifest binds repository, channel, rolling tag, package version, candidate
commit, and the four archive sizes/digests. It is staged, validated, promoted, recovered,
or rolled back as part of the same Release asset set. A legacy eight-asset rolling
Release remains a valid predecessor during the first manifest-producing transaction;
new candidates require the manifest.

The publisher and its transaction harness must remain compatible with the Bash 3.2
runtime provided by hosted macOS runners. Case normalization uses portable utilities,
and mock API paths must not expand empty arrays while `nounset` is active.

GitHub Release `PATCH` and `DELETE` operations do not expose a conditional-write
precondition. nib therefore adopts an exclusive-writer contract for rolling Releases:
the channel workflow's repository `GITHUB_TOKEN` is the only actor permitted to create,
retag, edit, or delete `prod-latest`, `development-latest`, or their staging and backup
Releases. The publication job runs through the channel-specific
`release-prod`/`release-development` environment and is the only workflow job with
`contents: write`. Repository administrators must not grant a PAT, deploy key, GitHub
App, reusable workflow, or manual operator a concurrent Release-write path. Emergency
intervention requires disabling the channel workflow first and reconciling its durable
staging/backup transaction before mutation. The local transaction fails closed on every
external retag it can observe; the exclusive-writer contract removes the API's
unfenceable proof-to-mutation interval from the supported operating model.

T010 remains in development until the exact committed workflow revision completes a
development-channel run and its published artifacts are inspected.

Official release builds expose `nib update`. It compares the embedded build commit with
the selected rolling channel manifest, reports a successful no-op when current, and
otherwise downloads, verifies, smokes, and safely replaces the current executable.
Local/source builds remain installer-managed. Eligible user-facing commands perform a
bounded, read-only startup check; `NIB_NO_UPDATE_CHECK=1` disables it, and protocol or
worker commands never emit update notices.

Before the first updater-capable production rollout, configure `release-prod` with a
required reviewer and leave the candidate's exact production publication unapproved.
Run two distinct successful development publications, with the second using that same
candidate SHA already present on `main`. Then manually dispatch
`.github/workflows/release-update-qualification.yml` from `development` with the
bootstrap and candidate Release Artifacts run IDs, bootstrap commit, and held production
run ID. The workflow verifies exact workflow provenance, ancestry, chronological
ordering, no intervening successful development publication, the current development
manifest, and the still-held same-SHA production run. Its read-only four-runner matrix
downloads the first run's native archives and requires each bootstrap binary to emit the
second commit's interactive notice, replace itself from the public
`development-latest` release, report the second exact identity, and leave its bytes
unchanged on a subsequent already-current update. A final job revalidates that the same
production deployment remains pending on `release-prod`, `main` and
`development-latest` still name the candidate, and `prod-latest` does not. Only after
the complete qualification succeeds may the exact held production deployment be
approved. The qualification workflow cannot mutate Releases or refs and does not weaken
the release workflow's exclusive-writer contract.

The Windows qualification creates its terminal with the operating system's inbox
`conhost.exe --headless` mode through a repository-owned bounded adapter. Unlike the
Git for Windows `winpty.exe` adapter, the inbox host does not require the Actions
PowerShell process to already have terminal handles. It accepts redirected input and
output pipes, creates the real child console, and returns that console's combined VT
stream. A repository child adapter runs the requested executable without redirecting
its handles and writes one unpredictable exit marker after it completes, so the outer
host can preserve an exact nonzero child status without trusting `conhost.exe`'s own
status or allowing child output to forge the marker.

Normal hosted Windows CI runs this smoke before the long Windows compilation and test
steps. It requires a PowerShell child to observe interactive standard error, captured
output, and its exact nonzero exit status. Every invocation remains inside a separately
supervised PowerShell process: output and diagnostics drain asynchronously, shutdown
has an internal deadline, and both supervisors kill the complete host process tree if
that deadline is exceeded. The same smoke forces a timeout with a lingering console
descendant and requires bounded cleanup with no surviving child. That resistant child
publishes its PID before cold handler compilation and a separate readiness signal only
after the console-close handler is installed. The probe accepts only the exact bounded
outer-host timeout inside a finite time window, so slow hosted startup cannot masquerade
as successful timeout cleanup and an early setup exit receives a distinct diagnostic.
After observing readiness, the probe publishes a separate armed signal and continuously
requires the descendant to remain alive until cleanup. The full timeout and cleanup
sequence has a 40-second end-to-end upper bound.

The publisher creates its reserved staging Git ref explicitly at the candidate SHA
before draft upload and supplies `--verify-tag`; it does not rely on a draft Release to
materialize that ref. If execution stops after ref creation but before the draft exists,
only an exact-SHA rerun may clean that unreleased stage (and its matching backup ref)
after revalidating the coherent prior rolling release.

GitHub may still asynchronously rewrite the private draft's tag to a generated
`untagged-*` value. The publisher treats only one exact marked draft for the channel as
the staged transaction, revalidating its immutable Release ID, target commit, draft
state, and transaction marker before cleanup, plus its complete asset set before any
forward or public mutation. A missing staging ref does not orphan that exact draft,
while multiple or mismatched untagged drafts stop publication for operator
reconciliation. Every tagged, untagged, and immutable-ID discovery read must succeed;
an ambiguous mutation is accepted only after a successful read proves its terminal
state.

Immediately after `gh release create` returns, GitHub's release list and asset views may
briefly lag that owned draft. Initial publication therefore waits for at most 12 attempts,
two seconds apart, for the draft and its exact uploaded nine-asset set. Once a Release
ID is observed, every later attempt pins and directly re-proves that same immutable ID,
exact private metadata, target commit, and transaction marker even if list discovery
temporarily goes empty. Only absence, a missing expected asset, or a not-yet-uploaded
expected asset is retryable. API read failure, multiple or changing IDs, ownership
drift, and unexpected asset names fail immediately; exhausting the bound fails closed
into normal transaction reconciliation.

For rollback-mode transactions, immutable-ID recovery also classifies the retained
rolling and backup state after a missing staging ref, so it can safely restore the
prior release or finish an already-started forward publication. A legacy pending draft
whose stage and matching backup refs were already cleaned can be finalized after exact
private-draft ownership is proven, but it is removed only while its recorded prior
rolling tag and Release remain coherent.

Pushes that change only `docs/**` and/or `agents/memory/**` do not start Release
Artifacts. This lets exact hosted rollout evidence be reconciled after publication
without replacing an already-qualified binary with a documentation-only commit.
