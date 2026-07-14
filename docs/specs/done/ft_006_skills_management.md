# FT-006: Skills Management CLI

**Status:** Done
**Related:** [T006](../done/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md)

## Scope

nib needs a set of CLI commands to manage skills. Currently, skills are managed by manually copying `SKILL.md` files into `~/.config/nib/skills/` or `.nib/skills/`. To improve the user experience, we need explicit CLI commands to list, install, and remove skills.

## Problem Statement

Users lack a streamlined way to discover, install, update, or remove skills from the CLI. This friction prevents easy adoption of community or organizational skills.

## Acceptance Criteria

- [ ] `nib skill list` - Lists all currently installed skills (both global and local).
- [ ] `nib skill install <url_or_path>` - Installs a skill from a local path or a remote URL into the global skills directory.
- [ ] `nib skill remove <name>` - Removes an installed skill by name.
- [ ] Ensure all commands handle errors gracefully (e.g., skill not found, invalid URL).

## Affected Areas

- `src/main.rs` (CLI argument parsing using `clap`)
- `src/skills/mod.rs` (or equivalent location where skills logic lives)
- `src/config/mod.rs` (for global/local paths)

## Validation Gates

- Pass `task check`.
- Pass `task test`.
- Manual verification: `nib skill install`, `nib skill list`, `nib skill remove` function as expected.
