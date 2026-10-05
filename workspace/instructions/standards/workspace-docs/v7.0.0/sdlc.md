# Spec-Driven SDLC (`workspace-docs@7.0.0`)

## Development Workflow

Non-trivial work has one categorized specification under
`workspace/specs/<state>/<primary-feature>/<spec>.md`. The default route is:

```text
backlog -> development -> test -> done
```

Use `develop` or the documented shared test target for integration and `main`
for completion. A project may explicitly document a direct-to-main route,
including urgent fixes, preserving every review, verification, acceptance,
and approval requirement. Branch names alone never prove a transition.
Deployment and package publication are separate release events.

Before implementation, a non-trivial spec defines scope, acceptance criteria,
affected areas, an implementation plan, validation gates, risks, Version
Impact, and Memory Impact initialized to `pending`. A clear user task request
approves its scope; ask only for material missing product decisions or a
required governed instruction approval. Small low-risk changes may proceed
without a spec.

## Standing Workflow Authority

A task request authorizes ordinary task-branch creation, selection, and pushing,
commits, pull-request updates, safe CI repairs, shared-test integration, merge
into main, and non-destructive release. Do not request repeated human approval
for these steps. Proceed automatically once all required gates pass.
Standing workflow authority does not include permission to modify governed
instructions. Governed instructions need prospective human approval for their named scope; a direct request naming the instruction change is sufficient. Destructive production actions also need scoped human approval exposing target, effect, recovery, and workflow.
Repository-host, branch, environment, and credential protections are
never bypassed. Silence or elapsed time is not approval.

## Spec States

The pinned [Workspace standard](../README.md) defines
versioned behavior. This project's successor contract is Workspace Docs 7.

| State | Meaning and transition evidence |
| --- | --- |
| `backlog` | Accepted inactive work awaiting implementation. |
| `development` | Implementation, review, and final-candidate verification are active. Local completion does not itself advance the state. |
| `test` | Confirmed integration into the configured shared test branch or environment; verify the actual combined revision and acceptance criteria. |
| `blocked` | Work cannot proceed or is deliberately deferred; record the previous stage, reason, kind, and resume condition. |
| `done` | Verified work merged into main, with acceptance, documentation, catalog, version records, and memory reconciled. Deployment or publication may still be pending. |

Normal transitions are `backlog -> development -> test -> done`.
A documented direct route permits `development -> done` only after the same
required gates and confirmed main merge. Block from backlog, development, or
test; resume at that recorded stage and renew stale review, verification, and
acceptance evidence. Use these fields in a blocked specification:

```text
State: blocked
Previous State: development
Block Kind: impediment
Block Reason: Required dependency is unavailable.
Resume Condition: Dependency becomes available and affected evidence is renewed.
```

`Block Kind` is `impediment` or `deferred`. Preserve blocked work and unique
commits. Gate failures alone do not require a blocked transition: repair and
rerun safely while development continues. Block only when a missing decision,
dependency, external action, or deliberate deferral prevents progress.
Done history is preserved; defects and enhancements create linked follow-up
specs rather than reopening completed work. No existing spec becomes done
solely because the completion definition changed.

Every current spec has one primary feature and exactly one catalog row with
its path, feature, state, and rationale. Update the spec, catalog, and release
member paths together. Record actual integration or main-merge evidence.

## Review Gate

Every change receives author self-review. Small low-risk behavior changes
with focused tests may use self-review alone. Other non-trivial changes need
independent agent or human review of the exact candidate revision. Changes to
security, data integrity, public interfaces, governed instructions, or release
controls always require independent review regardless of size. Repository
protections may require a human reviewer in addition.

Resolve every finding before integration: implement it or obtain the
reviewer's agreement to a documented rejection. Style and optional suggestions
also require resolution; no unresolved findings remain. Record the candidate,
reviewer, outcome, and finding resolutions. Relevant edits renew affected
review and acceptance evidence. Complete review and fixes before freezing the
candidate and running final verification.

## Modular Quality Gates

The system designs project-specific modules from repository evidence. Every
gate declares its purpose, Taskfile command, relevant source/test/configuration
and generated inputs, dependencies and downstream consumers, resource
constraints, pass condition, and freshness rules. Validate selectors with
positive and representative negative cases before relying on them. Gates must
fail loudly when required tools or dependencies are missing.

During development, select only gates affected by each coherent change,
including transitive consumers. Review fixes rerun their affected modules.
Do not repeatedly execute unrelated aggregate gates during iteration. Unknown
scope or dependencies select the safe aggregate fallback. Generated contracts
are classified by behavior and consumers, not by directory name.
Independent modules may run concurrently only with demonstrated isolation;
shared mutable resources require serialization or explicit isolation.

After implementation, artifact generation, documentation reconciliation,
review, and all fixes finish, freeze the candidate and run `task check`,
`task test`, and every applicable project-specific gate. One complete final run
is the normal target, not a cap on necessary reruns after defects or changes.

Full required coverage is mandatory for every final candidate, including small
changes. Reuse a passing result only when deterministic native tooling proves
all relevant inputs, locks, gate definitions, tools, and non-sensitive
execution conditions unchanged. Missing, failed, interrupted, or stale evidence
requires rerunning the affected gate; unknown freshness requires full fallback.
A gate result never proves inputs changed during its execution. Record concise
candidate-bound results, reuse reasons, and final coverage without raw logs.
Protected CI may independently require fresh execution.

## Integration and Acceptance Gates

Verify the actual combined shared-test revision; contributor-only results
cannot prove a different merged revision. Before main merge, verify the
resulting revision whenever relevant inputs differ from the tested candidate.
Required coverage may use proven-fresh evidence; repository protections remain
authoritative. A moving integration base invalidates affected evidence.

Every acceptance criterion has a recorded result and supporting test,
inspection, demonstration, or user-decision evidence. Agents may establish
objective acceptance. Subjective or materially ambiguous product decisions go
to the user. All required gates and acceptance criteria must pass before main
merge; failures are repaired, never waived to gain speed.

## Completion and Release Tracking

Done requires confirmed merge into main and reconciled acceptance criteria,
verification, documentation, catalog, version records, and memory impact.
Handoffs record the exact candidate, review and finding resolutions, gate
results or valid reused evidence, acceptance results, shared integration,
main merge, and remaining release status. Keep secrets, machine-local
information, and raw logs out of tracked evidence.

Deployment and package publication are tracked separately with their own
artifact identity, checks, release evidence, and recovery plan. An applied
version or main merge is not a published artifact. Non-destructive production
releases proceed automatically after required release gates and protections;
destructive production actions retain the scoped human approval boundary.

## Memory Impact Completion Gate

Every completed user-directed task or bounded work item classifies memory
impact as `updated` or `none`, with a rationale in the final handoff and spec
where applicable. Internal commands are not separate memory actions.
Use `updated` for durable decisions, facts, preferences, or consequential open
questions; append both the category entry and `workspace/agents/memory/changelog.md`.
Use `none` when no durable context changes and explain why. Pending memory may
remain only while genuinely unresolved; done requires a resolved classification.

## Spec Version Impact

Follow the [version reservation contract](./versioning.md)
and `workspace/releases.json`. Reserve before implementation; one owner applies
one bump at development start or merge before integration artifacts are built.
Check latest shared reservations before choosing targets and at handoff.
Require applied versions before test or done; publication separately marks
reservations released. Reuse already-applied shared versions without another
bump. Run `task versions:check` as part of complete final verification.

## Repository Automation and Commits

Configure repository-native CI to run the required `task check`, `task test`,
and relevant project gates for the actual candidate and integration revisions.
Shared-test and main merges obey configured host protections. If publication
uses a separate workflow, its checks do not establish automatic PR validation.
Document the repository-specific pipelines and environments in the CI guide.

Iterative checkpoint commits may rely on affected modules and do not establish
final acceptance. Complete final evidence is required before PR handoff or an
update presented for acceptance. Use conventional commit messages:
`type(scope): description`.
