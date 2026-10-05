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
