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
- scripts/install.sh and install.ps1 (modeled on skm).
- Self-update logic in src/updater.rs (checks tags prod-latest etc.).
- Build embeds NIB_BUILD_COMMIT / CHANNEL via build.rs.
