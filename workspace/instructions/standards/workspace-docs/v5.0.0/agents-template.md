# AGENTS.md Template

This template defines the generated context block that may be inserted into
project `AGENTS.md` files. Manual project guidance stays outside the block.

````md
<!-- AGENT-CONTEXT:START workspace-docs@5.0.0 -->
## Workspace Documentation Standard

This project follows `workspace-docs@5.0.0`.

### Required Reading

- `AGENTS.md`
- `DESIGN.md`
- `README.md`
- `workspace/instructions/tech/task.md`
- `workspace/instructions/tech/sdlc.md`
- `workspace/instructions/tech/project_structure.md`
- `workspace/specs/README.md`
- `workspace/instructions/standards/workspace-docs/AGENT_MIGRATION.md` for
  workspace adoption, updates, or structural repair
- Relevant conditional docs under `workspace/instructions/tech/`
- Relevant specs under `workspace/specs/`
- Project memory under `workspace/agents/memory/` when present

### Spec-Driven Development

Every non-trivial change must be tied to a spec in exactly one state:

- `workspace/specs/backlog/`
- `workspace/specs/development/`
- `workspace/specs/test/`
- `workspace/specs/done/`

Every non-legacy spec must be grouped by its single primary feature:

```text
workspace/specs/<state>/<primary-feature>/<spec>.md
```

The root `workspace/specs/README.md` defines feature categories and is the
canonical status catalog. Update its link, feature, state, and status rationale
whenever a spec is created, moves state, changes feature, is reopened, or
otherwise changes why it is in its state.

Allowed transitions:

- `backlog -> development`
- `development -> test`
- `test -> done`

`development` means implementation is active. Move to `test` only after the
implementation is integrated into the configured test branch or environment,
conventionally `develop`. Move to `done` only after release through the
configured production branch or environment, conventionally `main`. Branch
names are defaults; documented repository-local equivalents are allowed.

Do not infer a transition from the checked-out branch name alone. Record the
confirmed integration or release event in the spec and catalog rationale.

Do not start implementation until the development spec has scope, acceptance
criteria, affected areas, validation gates, and a `Memory Impact` section with
`Status: pending`.

### Documentation & Instruction Boundaries

- **Root Policy & Tokens**: `AGENTS.md` and `DESIGN.md` remain at the root.
- **System & Agent Instructions (`workspace/instructions/`)**: Static policies,
  architecture rules, standards, agent prompts, and skills. Do not modify them
  unless an authorized migration spec requires it.
- **Development Specs (`workspace/specs/`)**: Lifecycle-managed specs in
  `backlog`, `development`, `test`, `done`, or preserved `legacy` state.
- **Human & Project Documentation (`workspace/docs/`)**: Reference,
  architecture, and session work documentation.
- **Company Context (`workspace/company/`)**: Business, brand, design, domain,
  and strategy reference material.
- **Agent Memory (`workspace/agents/memory/`)**: Durable decisions, facts,
  preferences, open questions, and changelog.

Generated context stays between `AGENT-CONTEXT` markers. Manual project rules
stay outside generated blocks.

### Agent Memory

Classify every completed user-directed task or bounded work item before final
handoff. Internal commands and tool calls are not separate memory actions.

- Use `updated` for a new or changed durable decision, stable non-obvious fact,
  recurring preference, or consequential open question.
- Use `none` when the task creates no new durable context, with a rationale.
- For `updated`, append the durable entry to its category file and append a
  corresponding record to `changelog.md`.
- Development and test specs may retain `pending` only while the durable result
  is genuinely unresolved. Resolve to `updated` or `none` before `done`.
- State the classification and rationale in every final handoff.

Never store secrets, credentials, personal data, or transient scratch notes in
memory.
<!-- AGENT-CONTEXT:END -->
````
