# Migration Notes: workspace-docs@7.0.0

This major version redesigns completion, blocking, branch authority, review,
and modular quality gates. Read [the SDLC](./sdlc.md) and [process](./process.md),
then the [audit checklist](./audit-checklist.md), before making changes.

## Updating from 6.0

1. Create a scoped development migration spec and reserve version impact.
2. Preserve released standard packages, manual policy, existing worktrees,
   spec history, and unrelated project content.
3. Add workspace/specs/blocked/ and require Previous State, Block Kind,
   Block Reason, and Resume Condition for blocked specs. Backlog, development,
   and test can block and resume at their previous stage with renewed evidence.
4. Pin v7 and replace only generated context unless manual policy reconciliation
   is explicitly approved. Reconcile current technical guides, skills, profiles,
   validators, selection, and integrity records to the adopted contract.
5. Define done as verified main merge plus reconciled acceptance and records.
   Preserve prior done history. Review old test-state specs individually; do not
   mark them done solely because their source reached main. Record any genuine
   acceptance gaps or deferral through the new blocked contract.
6. Retain full final verification and design tested module selectors from actual
   dependencies and consumers. Unknown scope/freshness uses aggregate fallback.
   Complete review before final gates; reuse only proven-fresh evidence.
7. Document the shared test target (default) or explicit direct route. Urgent
   work preserves gates. Track deployment/publication independently from done.
8. Ordinary branches follow task authority. Preserve host protections and scoped
   human approvals for governed instructions and destructive production actions.
9. Reconcile version gates: test and done need applied versions, while publication
   alone marks release reservations released. Do not fabricate historical releases.
10. Run focused gates during changes, then complete review and run final Taskfile
    checks/tests and project-specific gates on the stable candidate.

## Rollback

Before release, restore the previous pin and only this migration's changes,
preserving blocked work, evidence, dirty worktrees, and unique commits. After
release, issue a successor version rather than rewriting released artifacts.
