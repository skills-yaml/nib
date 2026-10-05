# Tech Doc Template

Use this template for required and conditional files under `docs/tech/`.

```md
# [Topic]

Metadata:

- Standard: workspace-docs@1.2.0
- Status: static
- Owner: project
- Last reviewed: YYYY-MM-DD

## Scope

State when this doc applies and what it does not cover.

## Source Of Truth

Name the files, commands, services, or specs that define current behavior.

## Required Rules

List rules that agents and contributors must follow.

## Workflow

Describe the repeatable process for this topic.

## Validation

List commands or checks that prove changes are correct.

## References

Link to related project docs and specs.
```

## Static Versus Generated

Tech docs are static by default. Automation may create inventory reports or generated context blocks, but it must not rewrite project-owned tech docs unless explicitly requested.

