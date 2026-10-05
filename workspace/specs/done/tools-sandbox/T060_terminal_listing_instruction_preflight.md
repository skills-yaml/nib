# T060: Terminal Listing Instruction Preflight

**Status:** Done — locally implemented, reviewed, and verified.

State: done
Primary Feature: tools-sandbox

**Related:** [T058](T058_preflight_diagnostics_and_independent_read_progress.md).

## Problem

Repository analysis proposes `task --list` without `affected_paths`. The command
classifier does not recognize Task, so instruction preflight rejects the call
before shell execution. The rejection loses its missing-scope reason and becomes
`instruction_context_missing`, with irrelevant context-length recovery advice.
Preflight also rejects classified read-only commands because it accepts only Safe.
The end-to-end regression additionally reproduced exit 127: bwrap hides HOME and
therefore hides Task installed at `~/.local/bin/task`, although the host shell can
run it. Worktree preparation succeeds in both cases.

## Scope

- Recognize only exact Task listing forms (`--list`, `-l`, `--list-all`, `-a`).
  Classify these as Safe rather than ReadOnly because Task loads executable project
  configuration. Preserve worktree isolation and all execution policy gates.
- Admit existing classified ReadOnly commands through instruction scope discovery.
- Preserve the selected PATH Task executable through home isolation using a single
  read-only file mount. Keep other home files, sibling binaries, and credentials
  hidden; support executable symlinks without exposing their parent directory.
- Distinguish missing or empty terminal path declarations from unreadable project
  instructions, including persisted events, plan state, and plain/TUI recovery.
- Keep arbitrary Task execution, extra arguments, shell composition, and external
  path selection behind their existing scope and approval requirements.

## Non-Goals

No terminal worktree routing change, blanket Task auto-approval, context budget
change, historical session repair, verification-binding relaxation, or release.

## Affected Areas

Command classifier, instruction scope resolver, tool schema description, agent
preflight/reconciliation, interactive outcome rendering, regression fixtures,
Task verification entrypoint, and the architecture reference.
Sandbox home isolation and its deterministic and native bwrap regression tests.
Additive failure outcome/category; no persistence migration.

## Acceptance Criteria

- [x] Exact Task listings without affected paths reach terminal execution in the
  managed worktree and return the task inventory.
- [x] Classified ReadOnly commands pass scope discovery; opaque commands and
  extra Task arguments still require explicit affected paths.
- [x] Home-installed Task remains executable under bwrap while home files and
  sibling files stay hidden and the exposed executable stays read-only.
- [x] Missing or empty scopes produce actionable `affected_paths` guidance and
  `tool_scope_required`, without context-length advice or raw diagnostic leakage.
- [x] Mixed batches preserve independent reads; failed-only batches persist a
  blocked plan and report failure in live and reloaded plain/TUI views.
- [x] Focused regression tests, `task docs:check`, and `task verify` pass.

## Implementation Plan

1. Add failing classifier, scope-discovery, and runtime regressions.
2. Implement narrowly bounded listing recognition and distinct scope recovery.
3. Review spec compliance and security boundaries; run canonical gates.
4. Reconcile acceptance evidence, catalog, and memory, then move to done under
   this repository's pinned backlog/development/done lifecycle.

## Validation Gates

`task test:terminal-preflight`, `task docs:check`, and `task verify`. The focused
task exercises classification, instruction scope, sandbox home isolation and approval, live/reloaded
display, and the real agent-loop path. Full runtime, context, and interactive
suites run through the complete gate.

## Risks and Rollback

Task configuration can evaluate commands, so listing remains isolated and Safe;
no arbitrary flags or task names become recognized. Real instruction loading
failures retain their existing recovery. New outcome consumers must classify the
stop as failure. Rollback restores the classifier and preflight behavior without
changing persisted schemas or user worktrees.
Only the selected executable file is rebound after home masking, never its directory.
This preserves the existing PATH trust boundary without mounting private home data.

## Prior Memory Notes

Resolved: the stable listing, scope-recovery, and home-installed executable
contracts are recorded in `workspace/agents/memory/decisions.md` and its changelog. No
historical session identifiers or transient diagnostics are stored in memory.

## Alternatives and Rollout

Moving Task into a system directory is an operator workaround, but leaves home
installations broken. Mounting the whole local bin directory exposes sibling files.
A selected executable file mount preserves the existing home isolation boundary.
Blanket Task classification would admit unrelated targets without declared scopes;
the listing allowlist keeps that requirement intact.

The patch applies when a rebuilt nib binary is launched. Existing sessions retain
their audit history and can retry their same plan; no migration or automatic repair
is required. Publication is outside this local preparation task.

## Open Questions

None for the local fix. Native bwrap qualification applies on supported Linux hosts;
other platforms retain their existing execution route.

## Validation Evidence (2026-09-30)

- The pre-fix instruction-scope regression failed with the exact missing
  `affected_paths` rejection. The initial runtime regression then exposed exit
  127 under bwrap because the home-installed Task executable was hidden.
- `task test:terminal-preflight` passed classifier, instruction scope, approval
  isolation, live/reloaded display, executable symlink/read-only mount, native
  bwrap privacy, and agent-loop listing/recovery regressions.
- Final `task verify` passed strict static checks, 1,252 library tests, 93 CLI
  tests, all integration targets, and doctests. All 54 runtime tests passed,
  including actual Task inventory output from the managed session worktree.
- The first full gate hit an intermittent release-transaction fixture failure.
  `task test:installers` and the final full gate both passed all 42 installer
  tests without changing release scripts or installer tests.
- `task docs:check` passed all five documentation integrity checks.
- Spec compliance and quality/security self-review confirmed exact listing
  recognition, preserved policy/worktree boundaries, single-file read-only
  exposure, hidden siblings/home data, and auditable blocked scope failures.

Publication remains outside this local preparation task; the installed binary
must be rebuilt or updated before existing sessions use the correction.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | historical | Retrospective classification of the preserved pre-v7 outcome; no new bump or release identity is inferred. |

## Memory Impact

Status: none
Rationale: This historical outcome is preserved; migration adds no new durable decision for this spec. Existing dated memory evidence remains authoritative.
