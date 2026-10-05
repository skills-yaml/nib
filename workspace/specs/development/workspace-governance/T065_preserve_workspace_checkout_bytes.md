# T065: Preserve Workspace Checkout Bytes

**Status:** Development

State: development
Primary Feature: workspace-governance

## Scope

Resolve the Windows checkout-byte failure discovered during
[T062](T062_workspace_docs_v7_upgrade.md) main qualification. CI37344943162
passed library, CLI, runtime and binary qualification, but the final Workspace
positive fixture rejected modified source package AGENT_MIGRATION.md.
Preserve root generated context and Workspace text as LF at checkout, while
preserving the vendored standard as exact upstream bytes without conversion.
Exercise every governance module through a real forced-CRLF Git checkout and
run canonical governance early in Windows/macOS CI. Keep raw SHA-256 validation
and all existing hosted gates unchanged. No instruction content or runtime
interface change, and no paid provider calls.

## Acceptance Criteria

- [x] AC-1: Forced-CRLF checkout preserves generated context, Workspace text
  and upstream package bytes; ordinary text still exercises CRLF conversion.
- [x] AC-2: A real Git checkout fixture passes every governance module and
  rejects removal of checkout protection or modified upstream bytes.
- [x] AC-3: Windows and macOS run task workspace:check immediately after Task
  installation, retaining existing full tests, qualifiers and smoke assertions.
- [ ] AC-4: Independent exact-candidate review, affected native gates, full
  native verification and revised hosted Windows qualification pass.
- [ ] AC-5: Confirmed development integration and main merge precede done;
  catalog, version rationale and durable memory are reconciled.

## Affected Areas

.gitattributes, tests/workspace_governance.rs, .github/workflows/ci.yml,
this spec, catalog, durable memory and own peer work records.
Standard source files, integrity manifest, governance implementation, native
binary and release manifests remain unchanged.

## Implementation Plan

1. Reproduce the failure with a full committed repository checkout under
   core.autocrlf=true and core.eol=crlf, including an ordinary-text control.
2. Pin AGENTS.md and workspace text to LF; apply a more-specific -text rule
   to preserve vendored standard bytes exactly.
3. Run governance early on Windows/macOS and independently review the exact
   candidate; run affected and full native gates and all hosted main gates.
4. Reconcile actual integration/main evidence, catalog and memory.

## Validation Gates

task test:workspace, task docs:check, task check, task test:task-contract,
task verify, git diff --check, hosted Windows/macOS canonical governance,
full platform suites and exact-source release-binary qualification.
Strict Linux bubblewrap tests are required locally; no paid live calls.

## Risks and Rollback

The vendored rule must override text conversion so upstream hashes describe
actual bytes. The regression must demonstrate CRLF conversion outside the
protected scope and exercise all governance modules. Never normalize bytes
inside the SHA-256 validator or edit upstream files to match a checkout.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | none | none | Repairs Git checkout policy and offline qualification fixtures only; changes no native runtime or versioned public artifact. |

## Memory Impact

Status: updated
Rationale: New durable checkout-byte policy recorded in workspace/agents/memory/decisions.md and workspace/agents/memory/changelog.md.

## Independent Plan Review

Reviewer t062-review identified that a vendored-only rule would leave generated
context, spec sections and WIP frontmatter exposed to CRLF failures. Approved
LF protection for AGENTS.md/workspace, exact vendored bytes, full forced-CRLF
fixture, early native governance and justified none version impact.

## Local Validation Evidence

Committed candidate 2a7cfe7 passed task test:workspace: all eight cases, including
real forced-CRLF full governance, ordinary-text conversion, raw-byte tamper
rejection and protection-removal rejection. The initial negative fixture was
corrected to remove files before fresh checkout and assert actual CRLF bytes;
no pre-fix gate pass or final hosted acceptance is inferred.
