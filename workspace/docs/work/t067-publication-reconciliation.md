# T067: Development Publication Reconciliation

## Scope and Authority

The user selected the 0.3.0 publication-record follow-up on 2026-10-06.
This is the existing publication phase of
[T067](../../specs/done/interactive/T067_question_form_runtime_implementation.md),
whose completed runtime contract and release membership remain preserved.
The pinned versioning contract permits done specs with applied reservations and
requires publication to be reconciled separately with artifact evidence.

Update the nib-question-form release status and durable publication facts.
No new target or version bump, runtime change, instruction edit, workflow change,
production publication, or paid-provider qualification is part of this work.
One author uses an isolated ordinary task branch; independent review is read-only.
The primary checkout's existing two skill-pin edits remain outside this change.

## Confirmed Publication Evidence

- Repository: `skills-yaml/nib`.
- Channel/tag: `development` / `development-latest`.
- Version: `0.3.0`, existing nib-question-form target from baseline `0.2.0`.
- Source revision: `70acf355eb55621a0206b5070b2b6205e37502ed`.
- GitHub release publication: `2026-10-06T07:50:47Z`.
- [Release Artifacts 37430352520](https://github.com/skills-yaml/nib/actions/runs/37430352520)
  completed successfully, including four native archive builds and publication.
- [Public manifest](https://github.com/skills-yaml/nib/releases/download/development-latest/nib-release.json)
  SHA-256: `12d9fe361227b0651a32f4f34d83197059ecaf4992b8b9e29ae2fdc96f6de0f5`.
  This rolling URL can later advance; the recorded source and digests identify
  the inspected publication event rather than promising the alias is immutable.
- Manifest repository, channel, tag, version and commit match the release event.
  All four archive hashes, positive sizes and uploaded states match GitHub asset
  metadata; each portable checksum asset is present.

| Archive | Size (bytes) | Archive SHA-256 |
| --- | --- | --- |
| `nib-linux-x86_64.tar.gz` | 13,048,875 | `ff676d89c08e398138f16a21190caef2d5f3de7055470634207523abbadd0bbb` |
| `nib-macos-aarch64.tar.gz` | 11,201,687 | `3f82ec0ef89ca363f5916629a092b4bd3ea189794cdae349364783d4741a2a7a` |
| `nib-macos-x86_64.tar.gz` | 11,819,458 | `b03adba70d049ad2124caaf7e6ef052dd5108a830a17fb57e0e335efdb0bfae4` |
| `nib-windows-x86_64.zip` | 11,447,979 | `d5ca2ffb8470f8ae9ba27f5c6e44e01d978e950b203580223c8ebbe134403e6e` |

Archive hashes identify compressed public release assets; they are distinct from
qualified executable hashes. The source revision previously passed exact-source
Linux/macOS/Windows CI, including
[completion qualification 37425440774](https://github.com/skills-yaml/nib/actions/runs/37425440774)
and actual main CI 37430365236. This existing source qualification does not replace
fresh record verification for the current reconciliation candidate.

An earlier separately inspected 0.3.0 development publication identified qualified
implementation `a3b99632ae8cb452d5bfaff5555a3728999e8ba5`; the current receipt records
its subsequent completed-record publication at `70acf355` without another bump.
The ledger preserves earlier completion-preparation snapshots and nib-next 0.2.0
contract-only history; the new event supersedes the prior applied-only status.

## Production Boundary

The public production manifest was separately inspected and reports version
`0.1.0` at `15123a3ef275458efc87200400219aeacc3e9ea9`, channel/tag
`prod` / `prod-latest`. Released status for nib-question-form records development
publication. It does not claim production publication or authorize the protected
first updater-capable production rollout.

## Review and Verification Plan

Independent review approved this bounded publication-record scope before edits.
Obtain exact-candidate review of ledger/history preservation, artifact provenance,
channel distinction and memory before freezing the candidate. Run fresh `task verify`,
`task docs:check` and `task versions:check`; all Cargo operations remain serial.
Create an ordinary main-targeted PR and qualify its exact merge checkout through
native Linux/macOS/Windows CI before safe delivery of that same revision to shared
development and main. Record final verification and branch receipts in the PR
handoff so the frozen tracked candidate remains unchanged.

No new tests are needed for this factual record update. Existing native version,
Workspace, documentation and complete suite gates validate its affected consumers.
T061/T067 completion history and the catalog lifecycle remain unchanged.

## Memory Impact

Status: updated
Rationale: The newly verified public development publication is appended to
[workspace/agents/memory/facts.md](../../agents/memory/facts.md) and
[workspace/agents/memory/changelog.md](../../agents/memory/changelog.md).
No secrets, private session identifiers or raw local logs are stored.
