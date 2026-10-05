# AGENTS.md Template

This template defines the generated context block that may be inserted into project `AGENTS.md` files.

Manual project guidance must remain outside the generated block.

```md
<!-- AGENT-CONTEXT:START workspace-docs@3.0.0 -->
## Workspace Documentation Standard

This project follows `workspace-docs@3.0.0`.

### Required Reading

- `AGENTS.md`
- `DESIGN.md`
- `README.md`
- `workspace/instructions/tech/task.md`
- `workspace/instructions/tech/sdlc.md`
- `workspace/instructions/tech/project_structure.md`
- `workspace/instructions/standards/workspace-docs/AGENT_MIGRATION.md` for
  workspace adoption, updates, or structural repair
- Relevant conditional docs under `workspace/instructions/tech/`
- Relevant specs under `workspace/specs/`
- Project memory under `workspace/agents/memory/` when present

### Spec-Driven Development

Every non-trivial change must be tied to a spec.

Specs live in one state:

- `workspace/specs/backlog/`
- `workspace/specs/development/`
- `workspace/specs/done/`

Allowed transitions:

- `backlog -> development`
- `development -> done`

Do not start implementation until the development spec has scope, acceptance
criteria, affected areas, validation gates, and a `Memory Impact` section with
`Status: pending`.

### Documentation & Instruction Boundaries (Enforced Structure)

Documentation in the repository is divided into distinct operational areas:

- **Root Policy & Tokens**: `AGENTS.md` and `DESIGN.md` MUST remain at the repository root.
- **System & Agent Instructions (`workspace/instructions/`)**: Static, read-only policies, architecture rules (`workspace/instructions/tech/`), workspace standards (`workspace/instructions/standards/`), agent prompts (`workspace/instructions/agents/`), and skills (`workspace/instructions/skills/`). Agents MUST NOT modify these files unless explicitly instructed by a migration spec.
- **Development Specs (`workspace/specs/`)**: Active lifecycle feature specifications (`workspace/specs/backlog/`, `workspace/specs/development/`, `workspace/specs/done/`, `workspace/specs/legacy/`).
- **Human & Project Documentation (`workspace/docs/`)**: Reference documentation, architecture overviews (`workspace/docs/architecture/`), and session work logs (`workspace/docs/work/`).
- **Company Context & Reference (`workspace/company/`)**: Non-code business documentation, brand assets, design systems, domain glossaries, and strategy briefs.
- **Agent Memory (`workspace/agents/memory/`)**: Durable agent memory store (`decisions.md`, `facts.md`, `preferences.md`, `open-questions.md`, `changelog.md`).

Generated agent context must stay between `AGENT-CONTEXT` markers. Manual project rules must stay outside generated blocks.

### Agent Memory

Durable project memory belongs in `workspace/agents/memory/`.

Classify the memory impact of every completed user-directed task or bounded work
item before the final handoff. Internal commands and tool calls are not separate
memory actions.

- Use `updated` when the task creates or changes a durable decision, stable
  non-obvious fact, recurring preference, or consequential open question.
- Use `none` when the task creates no new durable context; include a concise
  rationale.
- For `updated`, append the durable entry to the appropriate category file and
  append a corresponding record to `changelog.md`.
- For non-trivial work, resolve the development spec's `Memory Impact` status
  from `pending` to `updated` or `none` before moving the spec to `done`.
- State the classification and rationale in every final handoff.

Record only stable facts, decisions, preferences, and open questions. Do not
store secrets, tokens, credentials, personal data, or transient scratch notes.
<!-- AGENT-CONTEXT:END -->
```
