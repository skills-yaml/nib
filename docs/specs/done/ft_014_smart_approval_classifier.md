# FT-014: Smart Approval Classifier

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Introduce an intelligent layer inside the `ToolExecutor` that automatically classifies the safety and risk of a given tool call, allowing for dynamic approval workflows.

## Problem Statement
At the feature baseline, approvals were largely binary (auto-approve or ask-user) and
based on static `ToolRegistry` metadata. The reconciliation below records the shipped
argument-aware deterministic classifier.

## Goals
- Enhance `ToolExecutor` to parse tool arguments and predict the potential blast radius.
- Implement rules engine tied to `AGENTS.md` (e.g., "Allow any git command that doesn't push").
- Automatically escalate to human-in-the-loop only when the classifier deems the action risky or out-of-policy.

## Scope
- Create `src/tools/classifier.rs` with deterministic logic to assess tool calls from
  their arguments. LLM-based classification requires a separate trust design.
- Modify `src/agent/loop.rs` or `src/tools/executor.rs` to intercept tool execution and call the classifier.
- If classified as `Safe`, bypass the user approval even if `auto_approve` is off for that tool category, or alternatively handle varying levels of risk (e.g. read-only commands vs destructive commands).
- Implement basic command safety classification (e.g., `git status`, `cargo test`, `ls` are safe).

## Acceptance Criteria
- `execute_tool` or `AgentLoop` queries the classifier before pausing for human approval.
- A hardcoded list of safe commands (like `cargo check`, `git status`) are auto-approved without pausing.
- Destructive commands (`rm`, `curl`, `wget`) are flagged for human approval.
- `task check` passes.
- Unit tests verify the classifier correctly flags safe vs unsafe commands.

## Affected Areas
- `src/tools/classifier.rs` (new)
- `src/tools/executor.rs` (execution path changes)
- `src/agent/loop.rs` (if approval logic is handled there)

## Validation Gates
- `task check`
- `task test`

## Reopened Audit (2026-07-15)

Scope: classify the registered `run_terminal` tool, parse shell composition safely,
apply AGENTS/skill policy rules, and test executor approval decisions end to end.

Affected areas: `src/tools/classifier.rs`, `src/tools/executor.rs`, context policy
loading, and classifier/executor tests.

Validation gates: safe/destructive/network/composition/policy tests, `task check`,
and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Classify registered terminal and management calls by arguments, reject unsafe shell
composition from auto-approval, and combine classifier results with AGENTS/skill policy.

### Acceptance Criteria

- [x] Safe read-only build/status commands can be classifier-approved.
- [x] Destructive, network, shell-composed, variable, and unscoped-path commands escalate.
- [x] Explicit deny/require/allow policy takes precedence over classifier defaults.
- [x] Approval source and decision are recorded in the session tool audit.
- [x] Agent lifecycle does not falsely advertise approval for classifier-safe terminal calls.
- [x] Final aggregate gates are green.

### Affected Areas

`src/tools/classifier.rs`, `src/tools/executor.rs`, `src/agent/loop.rs`,
`src/context/skills.rs`, and classifier/executor tests.

### Implementation Evidence

`classify_tool_call`, `classify_command`, and `safe_command_requires_isolation` feed the
policy/approval path in `ToolExecutor`.

### Validation Evidence

Classifier tests cover safe, composition, variable/path, destructive/network, and
management effects. Executor/E2E tests cover AGENTS and selected-skill policy denial.

### Validation Gates

- [x] Classifier unit and executor policy integration tests exist.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

The shipped classifier is deterministic rather than LLM-based. This is intentional for
fail-closed reproducibility; an LLM classifier would need a separate trust design.
