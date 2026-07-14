# FT-008: Smart Approval Classifier

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Introduce an intelligent layer inside the `ToolExecutor` that automatically classifies the safety and risk of a given tool call, allowing for dynamic approval workflows.

## Problem Statement
Currently, approvals are largely binary (auto-approve or ask-user) based on static metadata in the `ToolRegistry`. This doesn't scale for complex workflows where some terminal commands (`cargo check`) are perfectly safe, while others (`rm -rf`) are destructive. 

## Goals
- Enhance `ToolExecutor` to parse tool arguments and predict the potential blast radius.
- Implement rules engine tied to `AGENTS.md` (e.g., "Allow any git command that doesn't push").
- Automatically escalate to human-in-the-loop only when the classifier deems the action risky or out-of-policy.

## Scope
- Create a `tools::classifier` module (`src/tools/classifier.rs`) with a `Classifier` struct/logic to assess tool calls (e.g., matching command arguments against safe lists, or leveraging an LLM for classification).
- Modify `src/agent/loop.rs` or `src/tools/executor.rs` to intercept tool execution and call the classifier.
- If classified as `Safe`, bypass the user approval even if `auto_approve` is off for that tool category, or alternatively handle varying levels of risk (e.g. read-only commands vs destructive commands).
- Implement basic command safety classification (e.g., `git status`, `cargo test`, `ls` are safe).

## Acceptance Criteria
- `execute_tool` or `AgentLoop` queries the classifier before pausing for human approval.
- A hardcoded list of safe commands (like `cargo check`, `git status`) are auto-approved without pausing.
- Destructive commands (`rm`, `curl`, `wget`) are flagged for human approval.
- `cargo fmt && task check` pass.
- Unit tests verify the classifier correctly flags safe vs unsafe commands.

## Affected Areas
- `src/tools/classifier.rs` (new)
- `src/tools/executor.rs` (execution path changes)
- `src/agent/loop.rs` (if approval logic is handled there)

## Validation Gates
- `task check`
- `task test`
