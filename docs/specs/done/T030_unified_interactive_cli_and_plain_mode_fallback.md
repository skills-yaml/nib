# T030: Unified Interactive CLI and Plain-Mode Fallback

**Status:** Done
**Related:** [T025](../done/T025_interactive_chat_tui_capability_parity.md), [T028](../done/T028_current_session_first_tui_and_slash_command_completion.md), [FT-011](../done/ft_011_llm_streaming_and_tui.md)

## Summary

Present nib as one interactive product instead of two competing `chat` and `tui`
products. Running `nib` or `nib chat` should enter one interactive launcher that uses
the full-screen TUI when the terminal supports it and otherwise uses the line-oriented
plain renderer. Users may force either renderer explicitly. `nib run` remains the
non-interactive, one-shot interface, and `nib tui` remains a compatibility alias during
migration.

The two renderers continue to share the same agent loop, session store, command
registry, approvals, questions, model/provider configuration, skills, MCP operations,
and reconciliation rules. This task changes entry-point and presentation selection,
not workload authority.

## Problem Statement

nib currently advertises `nib chat` and `nib tui` as separate top-level interactive
interfaces. T025 made their agent capabilities equivalent because the two interfaces
had drifted, but users must still decide which product to start and documentation must
explain both command surfaces. The names also imply different execution semantics even
though both ultimately operate on the same profile-scoped session and agent loop.

The split has continuing costs:

- new interactive behavior must be verified through two public entry points;
- users may assume sessions, approvals, tools, or model behavior differ by command;
- basic installation guidance cannot offer one obvious interactive starting point;
- the richer TUI is not the default experience even on a capable terminal; and
- removing plain mode entirely would regress simple terminals, redirected output,
  accessibility workflows, diagnostics, and recovery from terminal incompatibility.

Codex provides a useful product model: one default interactive terminal command and a
separate non-interactive execution command. nib should adopt that clarity while
retaining its plain renderer as a supported mode rather than a second product.

## Product Decisions

- `nib` with no subcommand is the canonical interactive entry point.
- `nib chat` is an explicit spelling of the same canonical interactive launcher; it is
  not a separate line-mode product.
- Automatic mode prefers the full-screen TUI only when required input and output
  streams are terminals and a bounded terminal-capability preflight succeeds.
- `--plain` forces the line-oriented renderer. `--tui` forces the full-screen renderer.
  The options are mutually exclusive.
- An explicit `--tui` request fails with actionable guidance when the terminal cannot
  support it. It does not silently change the requested mode.
- `nib tui` remains a compatibility alias for `nib chat --tui`, preserving its existing
  `--run`, `--session`, and `--auth` behavior. Removing that alias is outside this task.
- `nib run "<goal>"` remains the one-shot and automation-oriented interface. Its
  approval, output, exit-code, and session behavior do not change.
- TUI and plain mode remain presentation adapters over shared interactive and workload
  services. Renderer-specific layout and input handling are valid; duplicated command
  grammar, session effects, or agent execution paths are not.
- T028 must be completed or explicitly reconciled before T030 implementation begins.
  T030 consumes its current-session-first TUI and shared completion work; it does not
  replace or weaken T028.

## Dependency Reconciliation (2026-08-19)

T028 is complete in this worktree. Its reconciled dependency surface provides one
shared interactive command registry, bounded chat/TUI completion, the current-session-
first TUI projection, preview-and-confirm session switching, strict target
revalidation, and shared command effects. T030 builds on those interfaces without
reverting or duplicating them.

The pre-implementation `task check` reached `cargo fmt -- --check` and stopped on one
formatting-only difference in the existing T028 chat changes. It did not establish a
behavioral failure or complete T028's remaining validation gates. The combined T028 and
T030 tree must pass formatting, compilation, tests, documentation checks, and the
terminal-focused gates before either spec may claim completion.

## Command Contract

```text
nib                                      # automatic interactive mode
nib --session <id>                       # resume in automatic mode
nib --auth                               # authenticate, then enter automatic mode
nib --run "<goal>"                       # submit an initial interactive goal

nib --plain                              # force line-oriented mode
nib --tui                                # force the full-screen TUI

nib chat [--plain|--tui] [--session <id>] [--auth] [--run "<goal>"]
nib tui [--session <id>] [--auth] [--run "<goal>"]  # compatibility alias

nib run "<goal>"                         # unchanged one-shot execution
```

Explicit subcommands such as `version`, `update`, `auth`, `doctor`, `config`, `task`,
and `mcp-server` continue to take precedence over the no-subcommand interactive
default. `nib --help` remains help and must not launch an interactive session.

### Deterministic mode selection

Mode selection occurs before authentication prompts, session creation or resume,
terminal raw-mode entry, worker creation, or agent execution:

1. Reject simultaneous `--plain` and `--tui` during CLI parsing.
2. Select plain mode when `--plain` is present.
3. Select TUI mode when `--tui` or the `nib tui` compatibility alias is present; fail
   with restoration-safe guidance if its preflight cannot succeed.
4. In automatic mode, select plain mode when required streams are not terminals or
   the terminal is explicitly non-interactive, such as `TERM=dumb`.
5. Otherwise, perform a bounded, side-effect-free TUI preflight and select the TUI on
   success. A preflight failure selects plain mode and prints one concise notice to
   standard error.

After terminal raw mode or an interactive workload has started, a TUI failure must
restore the terminal and report the error. It must not launch a second renderer or
resubmit a goal, because that could duplicate a plan, tool call, approval, or audit
record.

## User Experience

- Installation and quick-start documentation lead with `nib` for interactive work and
  `nib run` for one-shot work.
- A capable terminal opens the current-session-first TUI, including its composer,
  persisted timeline, live events, command completion, approvals, and questions.
- A pipe, basic terminal, unsupported terminal, or explicit `--plain` selection uses
  the line renderer with the same session and command capabilities.
- Help and startup text call these `tui` and `plain` presentation modes, not different
  agents or session types.
- Resuming the same session in either mode shows the same authoritative persisted
  workload. Switching presentation never clones, converts, or migrates a session.
- The `nib tui` compatibility alias may show a bounded deprecation notice directing
  users to `nib --tui`; the notice must not pollute protocol output or persisted
  conversation history.

## Scope

- Add a typed interactive presentation mode with `auto`, `plain`, and `tui` states.
- Make the no-subcommand `nib` invocation dispatch to the unified interactive launcher.
- Make `nib chat` use that launcher and accept `--plain`, `--tui`, `--run`, `--session`,
  and `--auth` consistently.
- Route the existing `nib tui` command through the same launcher as a compatibility
  alias without maintaining a second argument-to-runtime path.
- Implement side-effect-free terminal capability resolution and strict preflight versus
  post-start failure boundaries.
- Keep one shared interactive command registry, session resolution layer, stream event
  mapping, approval/question contract, and agent worker contract.
- Update README, user guide, help text, shell completions if present, architecture, and
  project-structure documentation to describe one interactive product with two modes.
- Add deterministic CLI, mode-selection, compatibility, terminal-restoration, and
  workload non-duplication tests.

## Non-Goals

- Removing either the TUI renderer or the plain renderer.
- Removing `nib tui` in the same change.
- Renaming or removing `nib run`.
- Making `nib run` allocate a terminal UI or changing its automation contract.
- Hot-switching renderers while an agent turn is active.
- Running multiple active sessions in one process.
- Changing session persistence, plan authority, worktree ownership, tool policy,
  approvals, provider transports, model configuration, skills, or MCP behavior.
- Adding a web, desktop, or IDE interface.
- Introducing terminal telemetry or a new UI framework.

## Compatibility And Rollout

- Existing `nib tui` invocations continue to force the TUI and preserve all accepted
  arguments.
- Existing non-terminal `nib chat` uses remain in plain mode through automatic
  selection. Human `nib chat` sessions on capable terminals intentionally move to the
  TUI; `nib chat --plain` preserves the old presentation explicitly.
- Primary documentation stops presenting `chat` and `tui` as equivalent choices.
  Migration notes show the exact replacement for each old command.
- The compatibility alias remains until a separate spec authorizes removal. Because nib
  is local-first and has no usage telemetry, this task must not claim evidence that the
  alias is unused.
- Release notes must identify the interactive default change and the plain-mode escape
  hatch before production publication.

## Workload And Safety Invariants

- Presentation mode is process-local and is not written into session, profile, project,
  or provider configuration.
- Mode resolution occurs before any new authoritative session or plan mutation.
- One submitted goal creates at most one agent worker and one run lease regardless of
  renderer initialization outcomes.
- Both renderers use the exact active session ID and shared command effects; neither may
  synthesize or cache an alternative workload state.
- Approval, question, cancellation, shutdown, and reconciliation remain native to each
  renderer but produce the same persisted decisions and terminal workload states.
- Every entered raw-terminal state is restored on normal exit, parse failure, worker
  failure, cancellation, panic boundary, and forced-mode preflight failure where entry
  occurred.
- Compatibility and deprecation notices are presentation output only and never become
  model context, session messages, lifecycle events, or tool audit records.

## Acceptance Criteria

- [x] Running `nib` with no subcommand enters the unified interactive launcher, while
      `nib --help` and explicit subcommands retain their existing behavior.
- [x] Running `nib chat` enters the same launcher and accepts the same interactive
      options as the no-subcommand form.
- [x] Automatic mode deterministically selects the TUI on a capable interactive
      terminal and plain mode for non-terminal or unsupported environments.
- [x] `--plain` always selects the line renderer; `--tui` always selects the full-screen
      renderer or fails before workload mutation with actionable guidance.
- [x] `--plain` and `--tui` cannot be supplied together.
- [x] `nib tui` forwards through the unified launcher, preserves `--run`, `--session`,
      and `--auth`, and cannot drift into a separate execution path.
- [x] A TUI failure after raw-mode entry restores the terminal and cannot cause plain
      mode to resubmit the initial goal or repeat any agent action.
- [x] TUI and plain mode expose the same shared slash-command grammar, session effects,
      model/provider operations, skills, MCP operations, approvals, questions,
      streaming semantics, cancellation, and reconciliation outcomes.
- [x] A session created or resumed in one mode can be resumed in the other without
      conversion, duplicated messages, or lost workload/audit state.
- [x] `nib run` behavior, output contract, approvals, exit codes, and session semantics
      remain unchanged.
- [x] Help, README, end-user guide, project structure, and architecture describe one
      interactive product and document `--plain`, `--tui`, and compatibility commands.
- [x] No new UI framework, persistence schema, provider behavior, or external telemetry
      is introduced.
- [x] `task docs:check`, `task check`, and an independent `task test` pass.

## Affected Areas

- `src/main.rs` — no-subcommand dispatch, unified typed arguments, compatibility alias,
  and help text.
- `src/chat.rs` — unified interactive launcher and plain presentation adapter, or a
  focused replacement module if naming would otherwise preserve the product split.
- `src/tui/mod.rs` — TUI preflight/launch boundary and restoration-safe errors.
- `src/interactive.rs` — presentation-neutral mode-independent command, session, and
  stream behavior; it must not absorb renderer-specific terminal code.
- `src/console.rs` — terminal/stream capability helpers only if no existing boundary is
  suitable.
- CLI and pseudo-terminal integration tests under `tests/`.
- `README.md`, `docs/user/guide.md`, `docs/tech/architecture.md`, and
  `docs/tech/project_structure.md`.
- Release notes or release-facing documentation for the default-interaction change.

The LLM adapters, agent loop, tool executor, sandbox, worktree manager, profile/session
formats, durable tasks, updater, and release transaction are not behaviorally modified.

## Implementation Plan

1. Complete or reconcile T028 so the intended TUI session and completion behavior is a
   stable dependency.
2. Introduce typed mode resolution and a side-effect-free terminal preflight with unit
   tests for explicit and automatic selection.
3. Create one interactive launcher that resolves mode before authentication, session,
   terminal, or worker side effects and dispatches to thin TUI/plain adapters.
4. Route no-subcommand `nib`, `nib chat`, and the `nib tui` compatibility alias through
   that launcher while retaining `nib run` unchanged.
5. Add pseudo-terminal and non-terminal regressions for startup, fallback, forced-mode
   failure, terminal restoration, argument forwarding, and exactly-once workload
   execution.
6. Update user, architecture, project-structure, help, and release-facing documentation.
7. Perform spec-compliance review, then code-quality review, and reconcile the spec only
   after all canonical and native terminal gates pass.

## Implementation Reconciliation (2026-08-19)

The implementation now has one typed launcher in `src/chat.rs`. Root `nib` and
`nib chat` pass the same `ChatArgs` contract to it, while `nib tui` translates its
legacy arguments once and forces the same launcher's TUI mode. Mode resolution is a
pure decision over the explicit flags and detected stream/terminal metadata and runs
before configuration, authentication, session resolution, terminal ownership, or
worker creation. A forced unsupported TUI returns actionable `--plain` guidance;
automatic redirected execution selects plain mode without polluting stdout.

The plain renderer retains the shared `src/interactive.rs` command/effect registry and
now accepts an exactly-once initial `--run` goal. The TUI performs a read-only preflight
before session mutation, initializes terminal ownership before resolving a session,
and uses one restoration guard that attempts both raw-mode and alternate-screen cleanup
on normal return, error, and unwind. There is deliberately no post-start fallback path,
so an initialization or worker failure cannot submit the same goal through plain mode.

`tests/interactive_cli.rs` exercises the compiled test binary for redirected automatic
selection, `nib chat --plain`, forced-TUI rejection before session mutation, help
precedence, and unchanged one-shot execution. `scripts/check-interactive-release.sh`
and `task smoke:interactive` exercise the optimized binary in real Linux pseudo-
terminals: automatic and forced selection, the compatibility alias, approval,
question response, cancellation reconciliation, normal exit, and alternate-screen
restoration. Ordinary Linux CI runs that smoke after building the release binary.

The spec-compliance review found every acceptance criterion represented in dispatch,
shared-runtime, executable, documentation, or pseudo-terminal evidence. The subsequent
code-quality review retained the existing Clap/Ratatui/Crossterm stack, kept workload
state out of presentation selection, removed duplicate terminal setup, and hardened
restoration so a failed explicit restore remains eligible for the guard's drop retry.

## Validation Evidence (2026-08-19)

- `task installers:check` passed, including syntax validation for the new smoke helper.
- `cargo test --test interactive_cli -- --test-threads=1` passed all three executable-
  level launcher tests.
- `task check:all-targets` passed for every local target and feature.
- Independent `task test` passed the complete serial unit and integration suite.
- `task check` passed installer validation, formatting, Clippy with warnings denied,
  compilation, and its complete serial test suite.
- `task build` produced the optimized release binary.
- `task smoke:interactive` passed the redirected and Linux pseudo-terminal release-
  binary matrix, including approval, question, cancellation, and restoration probes.
- `task docs:check` and `git diff --check` passed after documentation reconciliation.

Hosted PR run [32312998166](https://github.com/skills-yaml/nib/actions/runs/32312998166)
passed the exact implementation revision `cdc7df8cd5314f2624e3471eddae80195258b2c3`
on Linux, macOS, and Windows. The macOS and Windows jobs both passed all-target checks,
the complete test suite, the optimized release build, and release-binary smoke;
Windows also passed its bounded headless pseudoterminal adapter probe. The Linux
validation job passed the canonical checks, independent tests, runtime coverage,
optimized build, unified interactive launcher smoke, and managed-process owner-loss
smoke. This native evidence closes the remaining completion gate.

## Validation Gates

- Pure mode-resolution tests cover terminal/non-terminal streams, unsupported terminal
  metadata, preflight failure, explicit overrides, and conflicting flags.
- CLI parsing tests cover no-subcommand launch, help precedence, every shared option,
  explicit subcommands, and `nib tui` argument forwarding.
- Deterministic agent tests prove one initial goal creates one worker/run and that a
  post-start TUI failure cannot fall through to plain execution.
- Shared interactive contract tests continue to run against both renderers without
  duplicating command semantics.
- Linux pseudo-terminal smoke covers TUI auto-selection, forced plain mode, forced TUI,
  approval, question, cancellation, exit, and terminal restoration.
- Native Windows and macOS jobs exercise their existing terminal adapters and the new
  mode-selection boundary; unsupported capabilities must fail or fall back exactly as
  specified rather than being skipped silently.
- `task docs:check`.
- `task check`.
- Independent `task test`.
- `task check:all-targets` and the relevant native pseudo-terminal Task targets.
- A built release binary passes interactive smoke for `nib`, `nib chat --plain`,
  `nib --tui`, `nib tui`, and unchanged `nib run` before completion reconciliation.

## Risks And Mitigations

- **Breaking muscle memory:** Interactive `nib chat` changes from always plain to
  automatic TUI selection. Preserve `--plain`, retain both compatibility spellings,
  and publish exact migration examples.
- **False terminal detection:** Environment variables alone are not sufficient. Require
  real stream terminal checks plus a bounded capability preflight, with explicit flags
  taking precedence.
- **Duplicate execution after fallback:** Falling back after a worker starts can repeat
  mutations. Permit automatic fallback only before session/workload side effects and
  fail visibly after start.
- **Renderer drift:** A shared launcher alone does not guarantee parity. Retain the
  shared command/effect layer and behavioral contract tests introduced by T025/T028.
- **Terminal corruption:** Raw-mode failures can leave the shell unusable. Keep terminal
  ownership scoped, restoration idempotent, and covered by pseudo-terminal tests.
- **Compatibility alias permanence:** No telemetry exists to justify removal. Treat
  alias removal as a future explicit product decision, not an implicit cleanup step.
- **T028 overlap:** Both tasks touch interactive and TUI boundaries. Sequence T030 after
  T028 or rebase it onto T028's reconciled interfaces rather than duplicating or
  reverting current-session-first work.

## Alternatives Considered

### Keep `nib chat` and `nib tui` as equal products

Rejected as the long-term public model. T025 proves capability parity is possible, but
equal billing still creates user choice and documentation overhead without different
agent semantics.

### Remove plain mode

Rejected. Full-screen terminal control is not universally available or desirable, and
plain output remains valuable for accessibility, diagnostics, redirected streams, and
minimal terminals.

### Make TUI and plain mode different agents

Rejected. Presentation must not alter workload authority, tool policy, session state,
or reconciliation. Different agent implementations would recreate the drift T025
closed and make cross-mode resume unsafe.

### Replace `nib run` with interactive `--run`

Rejected. `nib run` has a clear one-shot and automation contract. Interactive `--run`
only seeds the first turn of an interactive session and does not replace non-interactive
execution.

## Completion State

Done. Implementation, local acceptance, canonical gates, Linux terminal smoke,
documentation, and the two-stage review are complete. Hosted PR run `32312998166`
confirmed the exact implementation revision on Linux, macOS, and Windows, including
native release-binary smoke and the Windows pseudoterminal boundary.
