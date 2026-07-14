# T010: Release Process

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
