# FT-006: Skills Management CLI

**Status:** Done
**Related:** [T006](../development/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md)

## Scope

Before this feature, skills were managed by manually copying `SKILL.md` files into
`~/.config/nib/skills/` or `.nib/skills/`. The shipped CLI provides explicit list,
install, and remove commands.

## Problem Statement

The baseline CLI lacked a streamlined way to discover, install, or remove skills. This
friction prevented easy adoption of community or organizational skills.

## Acceptance Criteria

- [x] `nib skill list` - Lists all currently installed skills (both global and local).
- [x] `nib skill install <url_or_path>` - Installs a skill from a local path or a remote URL into the global skills directory.
- [x] `nib skill remove <name>` - Removes an installed skill by name.
- [x] Ensure all commands handle errors gracefully (e.g., skill not found, invalid URL).

## Affected Areas

- `src/main.rs` (CLI argument parsing using `clap`)
- `src/skill_cmd.rs` and `src/context/skills.rs`
- `src/config/mod.rs` (for global/local paths)

## Validation Gates

- Pass `task check`.
- Pass `task test`.
- Manual verification: `nib skill install`, `nib skill list`, `nib skill remove` function as expected.

## Reopened Audit (2026-07-15)

Scope: make list/install/remove return structured errors, support local directories,
direct SKILL.md files, and remote Git/HTTP sources, then cover global/local behavior.

Affected areas: `src/skill_cmd.rs`, skill discovery/parser code, and CLI tests.

Validation gates: install/list/remove/error tests, `task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

List project/global skills and safely install/remove bounded local manifests,
directories, HTTP manifests, and Git repositories with declared resources.

### Acceptance Criteria

- [x] `nib skill list` classifies configured global and project-local roots.
- [x] Install supports directories, direct `SKILL.md`, bounded HTTP, and Git URLs.
- [x] Declared references/assets are preserved and parsed before atomic installation.
- [x] Remove validates skill names and reports missing/unsafe sources.
- [x] Command dispatch and error paths are tested without live remote dependencies.
- [x] Final aggregate gates are green.

### Affected Areas

`src/skill_cmd.rs`, `src/context/skills.rs`, `src/main.rs`, config paths, and skill tests.

### Implementation Evidence

`src/skill_cmd.rs` owns staging, bounded source preparation, atomic install/list/remove,
and safe names. `src/context/skills.rs` validates installed manifests/resources.

### Validation Evidence

Named tests in `src/skill_cmd.rs` cover directory/manifest/HTTP/Git install, declared
resources, classification, dispatch, unsafe names, missing environment, and HTTP failure.

### Validation Gates

- [x] Install/list/remove/error tests exist for every supported source class.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Remote skills are not signed or automatically trusted; users must review them. That
trust boundary is documented rather than hidden.

## Final Quality Review Remediation (2026-07-15)

### Scope

Bound local-directory, declared-resource, and Git skill installation by timeout,
depth, entry count, per-file bytes, and aggregate bytes before atomic publication.

### Acceptance Criteria

- [x] Local and cloned sources publish only the bounded declared-resource set; excess
  declared depth, entries, files, or bytes are rejected.
- [x] Declared references and assets share explicit per-file and aggregate copy bounds.
- [x] Git clone/checkout staging enforces live entry, depth, per-file, and aggregate byte
  budgets; partial-clone support is an optimization rather than the security bound.
- [x] Git command timeout/cancellation cannot hang on descendants retaining output pipes.
- [x] Manifest/reference reads prove the opened file identity across path checks, and
  configured `NIB_SKILLS_DIR` installs remain discoverable through `skill list`.
- [x] Every declared-resource path component is a real local directory/file; a symlinked
  ancestor cannot redirect parsing or installation outside the skill root.
- [x] Atomic publication staging is invisible to skill discovery until rename completes.
- [x] Git staging over-budget data remains unpublished and is terminated/cleaned at the
  active monitor boundary; per-file limits are additionally enforced by the child process
  where the platform supports it.
- [x] Oversized local and Git fixtures fail without leaving a partial installed skill.

### Affected Areas

`src/skill_cmd.rs`, `src/context/skills.rs`, and skill installation tests.

### Validation Gates

Focused local/Git/resource bound tests, `task test`, `task check`, and `task coverage`.

## Reopened Audit (2026-07-16)

### Scope

Make `nib skill list` complete within explicit discovery bounds and fail visibly
instead of returning a silent partial list or dropping malformed discovered manifests.

### Acceptance Criteria

- [x] Skill discovery reports whether file/entry/depth bounds prevented a complete scan.
- [x] `nib skill list` refuses to print a partial result when discovery is truncated.
- [x] A discovered malformed `SKILL.md` produces a contextual command error instead of
  disappearing from output.
- [x] Complete global/local listings remain deterministically sorted and classified.
- [x] Tests cover discovery overflow, malformed manifests, and normal mixed-root listing.

### Affected Areas

`src/context/skills.rs`, `src/skill_cmd.rs`, CLI output, and skill tests.

### Implementation Plan

1. Add a strict discovery API that reports entry, depth, and skill-count exhaustion.
2. Use strict discovery for installed-skill inventory while retaining bounded,
   best-effort runtime relevance loading.
3. Propagate malformed-manifest and incomplete-inventory errors through the CLI.
4. Preserve deterministic classification and sorting for complete scans.

### Risks

Making runtime skill selection fail on unrelated malformed ecosystem roots would be a
behavioral expansion. Strict completeness is therefore scoped to `skill list`; runtime
context discovery remains bounded and best-effort as documented.

### Completion Evidence

Strict inventory discovery reports entry, depth, and skill-count truncation; `skill
list` propagates those errors and malformed manifests instead of printing partial
state. It uses no-follow manifest inspection and rejects directory, dangling-symlink,
and special-file `SKILL.md` entries. Normal mixed global/local results remain
deterministically classified and sorted.

### Validation Gates

Focused listing completeness/error tests, `task check`, `task test`, and
`task coverage`.
