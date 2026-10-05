# Technical Documentation Template (`workspace-docs@6.0.0`)

Use this template for project technical reference documentation. Store the
result under the repository-declared human documentation root.

## Purpose

State what the component or workflow does and who relies on it.

## Architecture and Boundaries

Describe owned components, interfaces, data flow, trust boundaries, and the
authoritative implementation or instruction sources.

## Development and Validation

List repository-native setup and Taskfile entrypoints. If concurrent mutating
agents are supported, link to the repository's multi-agent coordination policy
and work-record location rather than duplicating its schema.

## Operations and Recovery

Document normal operation, observable failure states, rollback, recovery, and
safe cleanup. Use repository-relative paths and omit secrets or machine-local
details.
