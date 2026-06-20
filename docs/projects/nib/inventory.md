# nib Workspace Docs Inventory

Metadata:

- Adopted standard: workspace-docs@1.0.0
- Status: adoption inventory
- Owner: project
- Last reviewed: 2026-06-19 (specs aligned to new tech docs + workspace standard)

## Adopted Files

- `AGENTS.md`
- `README.md`
- `docs/tech/task.md`
- `docs/tech/sdlc.md`
- `docs/tech/project_structure.md`
- `docs/tech/backend_python.md`
- `docs/tech/ci.md`
- `docs/specs/README.md`
- `docs/specs/backlog/`
- `docs/specs/development/`
- `docs/specs/done/`
- `docs/standards/workspace-docs/README.md`
- `agents/memory/README.md`
- `agents/memory/decisions.md`
- `agents/memory/facts.md`
- `agents/memory/preferences.md`
- `agents/memory/open-questions.md`
- `agents/memory/changelog.md`

## Missing / Known Gaps

- Worktree had pre-existing dirty changes before adoption edits.
- No `.github/workflows/` directory exists although CI expectations are now documented.
- Existing source formatting drift blocks `task check`: `src/nib/skills/discovery.py`.

## Legacy Spec Paths (Aligned)

Foundational specs under legacy paths (`feature/`, `foundation/`, `task/`) have been **updated in place** (content, statuses, references, added implementation notes) to align with the new tech docs and workspace standard. Files were not relocated to preserve history and git context.

See `docs/specs/README.md` for details. Canonical states (`backlog/`, `development/`, `done/`) are now preferred for future specs.

## Quality Gates Available

- `task --list`
- `task check`
- `task test`

## Validation Run

- `git status --short` before edits: run; worktree already dirty.
- `task --list`: passed.
- `task check`: failed; Ruff format check would reformat `src/nib/skills/discovery.py`.
- `task test`: passed; 4 tests passed.

## Notes

- Existing project-specific manual instructions remain outside generated `AGENT-CONTEXT` markers.
- Legacy specs were preserved in place; foundational ones were **content-aligned** to tech docs and reality (see specs/README.md).
- No secrets or environment-specific credential values were added.

## 2026-06-19 Alignment + New Spec Run

- Created/updated `docs/specs/feature/ft_003_adopt_codex_sandboxing.md` (Symphony-style) describing **direct use of bwrap** inside nib's ToolExecutor for command sandboxing (Codex implementation used as reference for safe patterns and profiles). Preferred over full `codex sandbox` delegation for better integration.
- Added cross-references in FT-001 and architecture.md.

- Updated FT-001, FT-002, product.md, T001 with correct statuses, removed outdated "(to be created)", added Implementation Status sections, aligned tool descriptions and cross-refs to `docs/tech/*` (architecture, permissions, etc.).
- Updated `docs/specs/README.md` and this inventory to record the alignment.
- No file moves performed.
- This fulfills the explicit migration/alignment request while respecting "preserve legacy" guidance.
