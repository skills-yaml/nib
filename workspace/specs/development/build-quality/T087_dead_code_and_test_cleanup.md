# T087: Dead Code and Test Cleanup

**Status:** Development. Started on 2026-10-22 under a user-requested goal. It
replaces an uncommitted draft from another session (numbered T086 there, which
collided with the merged startup-screen spec); this spec is rewritten from the
draft's recorded findings.
State: development
Primary Feature: build-quality

## Problem and Evidence

Reviews of revision `82aadb7` found:

- unused exported Rust items in the library;
- tests that maintain an obsolete context and skill-selection path;
- three focused Task commands (`task test:tui-cache`, `task test:tui-shutdown`
  and `task test:mcp-stack`) that exit successfully with zero tests run,
  because the tests moved into submodules while the Task filters kept their
  old paths. The full suite still runs those tests, but not the MCP
  regression's dedicated one MiB stack execution.

Separately, Linux CI's interactive release smoke has repeatedly been killed by
the job timeout (exit 137) in the editor cases (`tui-editor-failure` twice,
`tui-editor-success` once) on unrelated pull requests; reruns passed.

## Scope

1. Point each focused Task filter at the tests' current paths, and make every
   focused test task fail when it runs zero tests.
2. Remove exported items that have no consumer in the crate, its binaries,
   tests or examples, unless they are documented public library API; remove
   or update tests that only keep the obsolete context/skill path alive.
3. Make the editor smoke cases wait for an observable signal instead of fixed
   delays, so a slow runner cannot hang them into the job timeout.

Out of scope: behavior changes, new features and refactors beyond removal.

## Affected Areas

`Taskfile.yml` (focused test tasks), library modules with unused exports and
their tests, `scripts/check-interactive-release.sh` (editor cases), and the
Task contract test if it checks filters.

## Acceptance Criteria

- [x] AC-1: `task test:tui-cache`, `task test:tui-shutdown` and
  `task test:mcp-stack` each run at least one test, and the MCP regression
  runs on its one MiB stack; a focused task that matches zero tests fails.
  Evidence: on `3891440` the three tasks ran 2, 3 and 1 tests (the MCP
  regression under `RUST_MIN_STACK=1048576`); the guard exited 1 with "the
  selection ran zero tests" for a stale `--lib tui::tests::no_such_test_`
  filter; every `--lib`/`--bin` focused filter in `Taskfile.yml` matched at
  least one listed test (the DOS short-name fixture is Windows-gated);
  `task test:task-contract` passed 4 tests.
- [ ] AC-2: Every removed item had no consumer; `cargo check --all-targets`,
  clippy with `-D warnings`, and the full test suite pass with no behavior
  change. Removed tests are listed with the reason. Evidence so far:
  `cargo check --all-targets` and `task check` (clippy `-D warnings`) pass;
  focused `context::`, `daemons::curator`, `daemons::cron`, `session::`,
  `llm::` and `profile::` lib tests (448) and the two rewritten
  `test_runtime_e2e` cases pass. The full suite runs at integration.
- [ ] AC-3: The editor smoke cases wait on observable output with bounded
  timeouts and pass repeatedly in local runs. Evidence so far: with a debug
  binary whose identity matched `3891440`, an editor-only harness (the
  script's own setup, consent case and both editor cases) passed 3/3 runs and
  5/5 runs under added CPU load; with the editor key removed the case failed
  after 26 s with "timed out waiting for TUI resumption after the editor" and
  a restored terminal instead of the 40 s PTY kill. The full interactive
  smoke runs at integration.
- [ ] AC-4: `task verify` with `NIB_REQUIRE_BWRAP_TESTS=1` and the interactive
  release smoke pass. Coordinator gate at integration.

## Implementation Plan

1. Route focused Task test commands through a zero-selection guard, repoint
   the stale filters, and enforce both in the Task contract tests.
2. Remove exported library items with no consumer, verified by a crate-private
   dead-code pass plus `cargo check --all-targets`, and remove or rewrite the
   tests that only kept the obsolete context/skill path alive.
3. Replace the editor smoke cases' fixed delays with bounded waits on terminal
   and session signals, pinned by the smoke contract test.
4. Hand off for integration gates (`task verify`, interactive release smoke).

## Implementation Notes

### Focused test guard (scope 1)

Every focused `cargo test` command in `Taskfile.yml` runs through
`examples/focused_test.rs` (built once by the internal `test:focused-runner`
task), which streams libtest output and fails when the command fails or when
its summary lines report zero executed (passed or failed) tests. The shared
parser lives in `tests/support/focused_test.rs` so the Task contract tests
exercise it. `task_contract_focused_test_commands_fail_on_empty_selections`
rejects any task that calls `cargo test` directly except the full suite and
the `--no-run` build. `test:tui-cache`, `test:tui-shutdown` and
`test:mcp-stack:small-stack` now select `tui::tests::render::`,
`tui::tests::session::` and `integrations::mcp_server::tests::test_part_1::`.
The repaired one MiB MCP run showed the request path had outgrown the stack
again; the tool-execution future is boxed at the MCP call boundary as T051
did for dispatch.

### Removed exported items (scope 2)

Found by compiling the library test build with every module made
crate-private (dead-code warnings list items nothing in the library or its
unit tests uses), then checking each against the binary, integration tests
and examples; `cargo check --all-targets` confirmed. No removed item appears
in user or reference documentation.

| Item | Reason |
| --- | --- |
| `context::assemble_context`, `assemble_runtime_context`, `agents::find_agents_md`, `format_context_for_prompt` | Pre-profile context assembly; runs use `assemble_profile_context` and `assemble_runtime_context_sections`. |
| `context::skills::{select_skill_files, relevant_skills, load_relevant_skills, skill_matches_task, automatic_skill_token_budget, parse_skill, SkillSelectionError, MAX_AUTOMATIC_SKILLS, SKILL_SELECTION_STOPWORDS_VERSION}`, the private scoring helpers and the `SkillSelectionSource::Automatic` variant | Obsolete automatic skill selection; skills load only as profile-active or explicitly invoked through `select_profile_skills`. |
| `agent::planner::{generate_plan, generate_plan_with_events, generate_plan_with_events_bounded}` | Context-free planning wrappers; the loop calls the context-aware planner. |
| `QuestionOutcome::answered_text`, `ExactRunSteeringHandle::{session_id, run_id}` | No caller. |
| `LlmConfig::update_model_for_active`, the `LLMConfigFile` alias | No caller; the alias's "used by CLI modules" note was stale. |
| `Cron::remove`, `Cron::run_maintenance`, `run_maintenance_with_policy`, `Curator::{new, new_with_policy, cleanup, cleanup_old_sessions, unpin_session, unpin_memory, unpin_skill}` | Legacy project-layout maintenance and unpin entry points; profile maintenance uses `run_profile_maintenance_due` and `cleanup_at`. |
| `DaemonAuditLog::path`, `TaskManager::mark_completed`, `Profile::context_path` | No caller (the context path field stays in use). |
| `TranscriptViewport::status_label`, `SessionResolution::notice` | No caller; the TUI uses its own notice and lifecycle labels. |
| `AnthropicClient::new`, `OpenAiCompatClient::{openai, xai, openrouter, meta}` | Convenience constructors; the factory builds clients from configured endpoints. |
| `LlmRequest::with_options`, `LlmResponse::with_usage`, `LlmError::with_retry_after_seconds`, `ToolDefinition::{description, parameters}` | No caller. |
| `MemoryStore::load`, `Plan::reject`, `SessionWorkspace::label`, `SessionStore::delete_if` | No caller; `delete_if`'s documentation moves to the crate-internal `delete_if_with_commit_check` it wrapped. |
| `tools::models::ExecutionRecord` | Unused record type. |

Retained: `ToolExecutor::with_policy_hook` has no consumer but is the only way
to attach the deliberately re-exported `ToolPolicyHook` extension trait;
removing it would remove a policy-evaluation path, beyond removal-only scope.
Enum variants of types in use (such as `ConfigMutation::Unchanged`) stay.

### Removed or rewritten tests (scope 2)

| Test | Change and reason |
| --- | --- |
| `configured_selection_is_authoritative_and_automatic_selection_is_ranked_and_limited`, `automatic_ties_use_canonical_path_and_duplicate_paths_are_selected_once`, `generic_words_and_one_repeated_description_token_do_not_select_a_skill`, `configured_selection_reports_missing_ambiguous_and_over_budget_skills`, `automatic_selection_respects_its_aggregate_prompt_allocation`, `non_selected_skill_does_not_load_its_missing_reference` (and helper `write_selection_skill`) | Removed: they only exercised `select_skill_files`, the obsolete automatic selection path. |
| `discovers_matches_and_builds_policy_rules` | Renamed `discovers_skills_and_builds_policy_rules`; the `skill_matches_task` assertions went with the function. |
| `runtime_sequence_selects_profile_context_and_skill_then_reconciles_audited_tools` | Rewritten to check the profile skill through `select_profile_skills` and `load_agents_md` instead of the removed helpers. |
| `assembled_context_and_scoped_core_tools_return_real_artifacts_and_errors` | Rewritten to use `assemble_profile_context` with an explicit `$context-proof` invocation and profile memory. |

### Editor smoke cases (scope 3)

The exit 137 at the case runner was the PTY bound's kill (`timeout -k 2 40`
sends KILL after TERM): the editor inputs slept a fixed 0.8 s, so on a slow
runner the input function gave up and never sent the quit keys, leaving the
TUI open until the 40 s limit. The cases now wait for the TUI's
bracketed-paste enable (raw input is live), its second occurrence (the TUI
resumed after the editor), the edited draft and the goal session under one
25 s deadline, and quit the TUI with a named diagnostic when a signal is
missed. The installers contract test pins these waits. The script has no
single-case mode, so local runs used a harness generated from the script's
own setup, consent case and editor section.

### Environment note

`workspace/specs/backlog/` holds no tracked file on this base, so a clean
checkout fails `task check` (`workspace:check` structure). Local validation
created the empty directory without committing it; the coordinator's pending
backlog spec restores it.
`task docs:check` also fails on this base because the T082, FT-021 and FT-022
development specs lack an implementation or rollout plan heading; this spec
now has one.

## Validation Gates

Focused Task commands while iterating; `task check`, `task test:task-contract`
and frozen `task verify` before handoff.

## Risks and Rollback

Removing an item a downstream library user relies on: keep documented public
API, and remove only items with no consumer and no documentation. Each removal
reverts independently.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-goal-minor | Cleanup with no behavior change; joins the goal minor 0.6.0 from published 0.5.2. |

## Memory Impact

Status: pending
Rationale: Classified when the cleanup is verified; a durable fact (focused
test tasks must fail on zero tests) may result.
