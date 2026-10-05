# Audit Checklist (`workspace-docs@7.0.0`)

## Lifecycle and Authority

- Required root files, Workspace directories, and blocked directory exist.
- Every current spec has one feature/state, declared state, and catalog row.
- Blocked specs have previous state, kind, reason, and resume condition.
- Shared-test integration is confirmed before test; verified main merge and
  reconciled acceptance/records are confirmed before done.
- Direct routes are explicitly documented and preserve every required gate.
- Done history is preserved; follow-up work is linked rather than reopened.
- Ordinary branches follow task authority and host protections; governed
  instructions and destructive production actions retain scoped human approval.
- Release/publication evidence is separate from main merge and applied versions.

## Review and Modular Verification

- Every change has self-review; non-trivial work has independent exact-candidate
  review except small low-risk changes with focused tests. Sensitive areas always
  have independent review, with human review when protections require it.
- Every finding is implemented or has reviewer-agreed documented rejection.
- Project gate modules declare commands, inputs, dependencies/consumers,
  resources, pass conditions, and freshness rules; selectors have negative cases.
- Iteration and review fixes run affected modules; unknown scope uses safe fallback.
- Artifacts, documentation, review, and fixes precede the frozen final candidate.
- Full required task check/test and project-specific coverage passes on it.
- Reuse has proven freshness; edits during runs cannot produce passing evidence
  for unchecked inputs. Missing tools, failed/interrupted runs, and unknown
  execution conditions cannot pass.
- Independent gates may run in parallel; shared resources are serialized/isolated.
- Actual combined test and resulting main revisions have required coverage.
- Every acceptance criterion has supporting result evidence; subjective decisions
  are confirmed by the user. Handoffs link concise evidence without raw logs.

## Versioning, Memory, and Coordination

- Specs have valid per-component version impact; reservations have one owner,
  collision-free targets, synchronized members, and applied versions before
  integration artifacts/test/done. Publication marks reservations released.
- Run task versions:check as part of complete final verification.
- Completed tasks classify memory as updated or none with rationale; updated
  records reference category and changelog. Blocked active work retains memory
  obligations and pending only while genuinely unresolved.
- Concurrent mutating agents have isolated linked worktrees, bounded shared
  scopes, independent review, serialized integration, and recoverable work.
- Runtime boards stay local and untracked; the primary checkout stays
  coordination-only during concurrent mutation. Cleanup preserves unique work.
- Generated context matches the pinned template; released packages stay unchanged.
