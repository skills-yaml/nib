# nib Spec Versioning

The [pinned versioning contract](../instructions/standards/workspace-docs/v7.0.0/versioning.md)
applies using Cargo.toml as the native version source and workspace/releases.json
as the ledger. task versions:check validates TOML package versions and reservations.
At 0.y.z use minor for additions/incompatible changes and patch for compatible fixes.
The released nib-next reservation (0.2.0, merge timing) records T062, its linked
T063 security repair, and T061 accepted-contract documentation only. Its bump
was applied once before integration artifacts; development publication is
confirmed at 8bc243d00ec3ec82e7287c704fcdd897ff78204a. Production publication
is separate. T061 runtime implementation remains pending and requires a fresh
atomic reservation from the latest published baseline before implementation,
with membership reconciled or a linked implementation spec. Released 0.2.0
cannot authorize future T061 code. Applied versions and publication are separate. Backlog proposal-only artifacts declare none until
implementation is approved and reserved. Check remote published state and current
shared reservations again before integration/publication.

Main delivery is verified at 4b0245b9890bb15a4c99ff46ec7296d42bf9401f via PR43
on 2026-10-05T20:20:23Z after final development integration
30e9ecf8151768ca236367ed4984e1a06bc96380 via PR45. Current exact-source native
and all-platform qualification passed without another bump. Lifecycle record
reconciliation preserves the released 0.2.0 target and documentation-only T061
membership; publication remains a separate verified event.
