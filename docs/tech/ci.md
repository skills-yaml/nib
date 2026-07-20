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

Publication uses fixed per-channel staging and backup refs plus a versioned marker in
the staged Release body. The publisher verifies the four exact archives, their four
portable checksum manifests, uploaded asset state, positive asset sizes, source-branch
lease, and rolling-tag lease before promotion. A later run reconciles an interrupted
transaction before it considers its own SHA. Recovery preserves all artifacts until a
complete forward or rollback state has been re-read.

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

The CLI does not currently expose a self-update command; rerun the installer for the
selected rolling channel.
