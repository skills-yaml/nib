# FT-008: Smart Approval Classifier

**Status:** Backlog
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Introduce an intelligent layer inside the `ToolExecutor` that automatically classifies the safety and risk of a given tool call, allowing for dynamic approval workflows.

## Problem Statement
Currently, approvals are largely binary (auto-approve or ask-user) based on static metadata in the `ToolRegistry`. This doesn't scale for complex workflows where some terminal commands (`cargo check`) are perfectly safe, while others (`rm -rf`) are destructive. 

## Goals
- Enhance `ToolExecutor` to parse tool arguments and predict the potential blast radius.
- Implement rules engine tied to `AGENTS.md` (e.g., "Allow any git command that doesn't push").
- Automatically escalate to human-in-the-loop only when the classifier deems the action risky or out-of-policy.
