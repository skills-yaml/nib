# T083: Progressive Skill Discovery and Management

**Status:** Development
State: development
Primary Feature: skills-mcp

## Problem and Scope

User authorized Codex-style skill management after read-only analysis on 2026-10-09.
Repository SKM skill folders are symlinked under `.agents/skills`; current discovery
omits them and rejects linked folders. Listing and runtime discovery disagree.
Replace automatic keyword body injection with a bounded metadata catalog and
controlled on-demand activation. Preserve existing nib install/remove commands,
profile active skills, execution restrictions, hooks and authoritative session audit.
Related historical contracts: [T006](../../done/skills-mcp/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md)
and [FT-006](../../done/skills-mcp/ft_006_skills_management.md).

## Behavior and Compatibility

Discover repository `.agents/skills` from launch directory to Git root, user
`.agents/skills`, nib user/admin locations, configured/profile/managed roots and
legacy nib/Grok roots. Resolve linked skill folders with bounded traversal,
canonical target deduplication and load-time identity revalidation; do not allow
resource symlinks or arbitrary filesystem reads. Duplicate names remain separate
catalog entries and require an exact catalog path for ambiguous activation.
Expose names/descriptions/paths to the model with a bounded catalog; load bodies
only for explicit `$name`, profile activation, or the `load_skill` tool. Remove
the three-skill automatic keyword limit from the runtime selection path.
`read_skill_resource` reads bounded regular files beneath an activated skill root.
Activation installs restrictions/hooks and persists usage before success.
Per-manifest enable/disable controls live in nib config; optional
`agents/openai.yaml` implicit-invocation policy is honored. Explicit invocation
continues to work for implicit-disabled skills. Refresh catalog each user turn.
Keep skill creation as ordinary authoring of a SKILL.md directory; no new registry,
plugin marketplace, external connectors or built-in workflow library is added.

## Affected Areas

src/context, src/config, src/skill_cmd.rs, src/tools, src/agent/loop,
src/interactive, associated tests, README and user documentation.

## Ordered Plan

1. Unify bounded discovery, canonical linked-folder validation, catalog and controls.
2. Implement explicit selection, bounded metadata prompt and audited activation tools.
3. Add command controls and selector, tests and documentation; reconcile memory.
4. Independently review exact candidate; run affected gates then task verify.

## Acceptance Criteria

- [ ] AC-1: Repository and user skills including SKM links are discovered through one shared catalog; canonical duplicates are collapsed, name collisions retained.
- [ ] AC-2: Model sees bounded metadata and loads relevant bodies on demand; explicit invocation and profiles load deterministically without keyword ranking.
- [ ] AC-3: Activation installs policies/hooks and authoritative usage; supporting files are bounded to activated roots and arbitrary paths rejected.
- [ ] AC-4: Enable/disable controls, implicit policy, CLI and plain/TUI selection share behavior; catalog refreshes between turns.
- [ ] AC-5: Tests cover success, errors, symlinks, ambiguity, drift, budgets, policies and audit; documentation explains compatibility.
- [ ] AC-6: Independent review and task verify, docs:check and versions:check pass before handoff.

## Validation Gates

Affected context, executor, interactive and CLI tests through Task; task check,
task verify, task docs:check, task versions:check. Independent security and
interface review required. Local validation alone retains development state.

## Risks and Rollback

External skill links are read-only capabilities for exact discovered roots, never
an expansion of general tool filesystem scope. Revalidate metadata and contents
on activation; fail closed on malformed metadata, drift or audit failure. Hooks
retain ordinary terminal approval. Rollback restores legacy heuristic selection.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-catalog-refresh | Additive skill catalog/tools/controls share the already-applied unreleased minor reservation; atomic transaction rechecked, no second bump. |

## Memory Impact

Status: updated
Rationale: Record the shared skill discovery and progressive activation contract in [facts.md](../../../agents/memory/facts.md) and
[changelog.md](../../../agents/memory/changelog.md).
