# Workspace Process & Governance (`workspace-docs@2.1.0`)

This document defines the process rules and operational boundaries for projects adopting `workspace-docs@2.1.0`.

## 1. Operational Areas

- **Root Policy & Tokens**: `AGENTS.md` and `DESIGN.md` reside at the repository root.
- **Root Container Directory (`workspace/`)**: All domain directories (`agents/`, `instructions/`, `specs/`, `docs/`, `company/`) reside under `./workspace/`.
- **System Instructions (`workspace/instructions/`)**: Read-only policies and architecture rules during feature development.
- **Development Specs (`workspace/specs/`)**: Spec-driven lifecycle tracking (`backlog` $\rightarrow$ `development` $\rightarrow$ `done`).
- **Human Docs (`workspace/docs/`)**: Current-project reference documentation,
  architecture notes, and session work logs.
- **Company Context (`workspace/company/`)**: Business reference materials, brand guidelines, design assets, domain glossaries, and strategy briefs.
- **Durable Memory (`workspace/agents/memory/`)**: Stable facts, decisions, preferences, open questions, and changelog.
