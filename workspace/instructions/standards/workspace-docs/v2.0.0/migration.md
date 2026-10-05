# Migration Guide: Moving to workspace-docs@2.0.0

This guide details how to migrate a workspace from `workspace-docs@1.x.x` to `workspace-docs@2.0.0`.

## Key Changes in 2.0.0

`workspace-docs@2.0.0` introduces a strict top-level 3-tier organizational structure:

1. **Root Guidelines**: `AGENTS.md` and `DESIGN.md` remain at the project root (`/AGENTS.md`, `/DESIGN.md`).
2. **`instructions/`**: Tech stack architecture standards (`instructions/tech/`), workspace standards (`instructions/standards/`), agent prompts/roles (`instructions/agents/`), and skills (`instructions/skills/`). Read-only for agents during standard feature development.
3. **`specs/`**: Development feature specs managed via lifecycle states (`specs/backlog/`, `specs/development/`, `specs/done/`, `specs/legacy/`).
4. **`docs/`**: Current-project reference documentation, architecture overviews
   (`docs/architecture/`), and session work logs (`docs/work/`).
5. **`agents/memory/`**: Durable agent memory store (`decisions.md`, `facts.md`, `preferences.md`, `open-questions.md`, `changelog.md`).

---

## Migration Steps

### Step 1: Retain Root Files
Do not move `AGENTS.md` or `DESIGN.md`. Ensure both reside at the repository root.

### Step 2: Create Directory Structure
```bash
mkdir -p instructions/tech instructions/standards instructions/agents instructions/skills
mkdir -p specs/backlog specs/development specs/done specs/legacy
mkdir -p docs/architecture docs/work
```

### Step 3: Relocate Tech Specs and Standards
```bash
# Relocate tech specifications
mv docs/tech/* instructions/tech/
rmdir docs/tech

# Relocate process standards
mv docs/standards/* instructions/standards/
rmdir docs/standards
```

### Step 4: Relocate Specifications
```bash
# Relocate active specs
mv docs/specs/backlog/* specs/backlog/ 2>/dev/null || true
mv docs/specs/development/* specs/development/ 2>/dev/null || true
mv docs/specs/done/* specs/done/ 2>/dev/null || true

# Relocate legacy specs
mv docs/specs/feature specs/legacy/ 2>/dev/null || true
mv docs/specs/foundation specs/legacy/ 2>/dev/null || true
mv docs/specs/task specs/legacy/ 2>/dev/null || true

# Remove old docs/specs directory
rm -rf docs/specs
```

### Step 5: Update Root References in `AGENTS.md`
Update your `AGENTS.md` header block:
```md
<!-- AGENT-CONTEXT:START workspace-docs@2.0.0 -->
## Workspace Documentation Standard

This project follows `workspace-docs@2.0.0`.

### Required Reading

- `AGENTS.md`
- `DESIGN.md`
- `README.md`
- `instructions/tech/task.md`
- `instructions/tech/sdlc.md`
- `instructions/tech/project_structure.md`
- Relevant conditional docs under `instructions/tech/`
- Relevant specs under `specs/`
- Project memory under `agents/memory/` when present
```

### Step 6: Record Decision in Agent Memory
Record a decision entry in `agents/memory/decisions.md` documenting adoption of `workspace-docs@2.0.0`.
