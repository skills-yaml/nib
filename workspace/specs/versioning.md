# nib Spec Versioning

The [pinned versioning contract](../instructions/standards/workspace-docs/v7.0.0/versioning.md)
applies using Cargo.toml as the native version source and workspace/releases.json
as the ledger. task versions:check validates TOML package versions and reservations.
At 0.y.z use minor for additions/incompatible changes and patch for compatible fixes.
Current T061 and T062 intentionally share nib-next (0.2.0, merge timing).
T062 delivery applies that reservation once before integration artifacts; T061
implementation remains pending. Applied versions and publication are separate. Backlog proposal-only artifacts declare none until
implementation is approved and reserved. Check remote published state and current
shared reservations again before integration/publication.
