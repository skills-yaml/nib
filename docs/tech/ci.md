# CI for nib (Rust CLI)

Follows skm project structure.

## Pipeline
- Use `task check`, `task test`, `task build`.
- Rust toolchain via dtolnay/rust-toolchain.
- Task via arduino/setup-task.
- Cross-platform release builds for linux/macos/windows.
- Channels: prod (main), development.
- Installers in scripts/ consume the release artifacts.

See .github/workflows/ci.yml and release.yml (modeled directly on skm).

## Taskfile
See root Taskfile.yml for check/fix/test/build (cargo based, with Python support).

## Install & Update

### End-user Installation

Use the platform-specific installers from the release artifacts:

**Linux / macOS**
```bash
# Stable releases
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh

# Development channel
NIB_CHANNEL=development curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh
```

**Windows (PowerShell)**
```powershell
$Repo = "skills-yaml/nib"
iex ((New-Object System.Net.WebClient).DownloadString("https://raw.githubusercontent.com/$Repo/main/scripts/install.ps1"))
```

Installers:
- Download the correct asset (`nib-linux-x86_64.tar.gz`, `nib-macos-aarch64.tar.gz`, etc.)
- Install to `~/.local/bin` (or `%USERPROFILE%\.local\bin` on Windows)
- Print PATH hint if needed

See:
- `scripts/install.sh`
- `scripts/install.ps1`
- `scripts/first-time-setup.sh` (runs `nib auth` after install)

### Development / Building

```bash
task build          # Release binary
task dev            # check + test + build + --help
cargo run -- chat   # Run without installing
```

The release workflow (`.github/workflows/release.yml`) builds for multiple targets and uploads to GitHub Releases using `prod-latest` / `development-latest` tags.

Self-update (future): `src/updater.rs` uses `git ls-remote` against the channel tags + embedded `NIB_BUILD_COMMIT` / `NIB_BUILD_CHANNEL` (from `build.rs`).
