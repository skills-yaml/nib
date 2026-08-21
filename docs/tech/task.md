# Task Runner (Taskfile)

nib uses [Task](https://taskfile.dev/) as the standard interface for all local and CI operations. This mirrors the convention across the workspace (revized, autonomus, skm, flirtyr, etc.).

## Rules

- Every repeatable command that a human or agent would run belongs in a Taskfile.
- Root `Taskfile.yml` is the entry point. Use `includes:` for subprojects (backend, fe, deployment, etc.) when they exist.
- Agents and CI must invoke tasks rather than raw commands (e.g. `task check`, `task test`, not direct `ruff` or `pytest`).
- Keep task names stable and descriptive (`check`, `test`, `fmt`, `build`, `deploy`, `coverage:report`, scoped variants like `backend:check`).

## Current minimal tasks (see root Taskfile.yml)

- `task` or `task default` — list tasks
- `task check` — installer checks, Rust formatting, Clippy, compilation, and the full serial test suite
- `task check:all-targets` — type-check every Rust target and feature (optionally for `TARGET`)
- `task fmt` — format Rust source
- `task test` — run the full Rust unit and integration suite serially
- `task test:durable` — run detached background-task and scheduled-worker process tests
- `task test:managed-process-capability` — verify the exact managed-process backend probe independently
- `task test:updater` — run self-update and update-notification unit tests
- `task test:doctor` — run doctor diagnosis, repair, and CLI persistence tests
- `task qualify:release-update:unix` — on a native Linux/macOS release runner, install
  a supplied development bootstrap artifact and prove notice, replacement, and no-op
- `task qualify:release-update:windows` — run the equivalent qualification against a
  supplied development bootstrap artifact on a native Windows release runner
- `task test:windows-pseudoterminal` — prove the bounded inbox Windows headless-console
  adapter creates an interactive child terminal and preserves output and exit status
- `task test:installers` — run installer and release-transaction integration tests
- `task test:llm-live:offline` — run the credential-free live-harness parsers, planner,
  matrix, report, and CI-contract tests
- `task test:llm-live:catalog` — discover and reconcile a provider's live model catalog
- `task test:llm-live:canary` — run paid qualification against provider defaults and
  the approved OpenRouter allowlist
- `task test:llm-live:selected` — run the reviewed nib task suite against every exact
  provider/model entry in `tests/fixtures/llm_live/selected_models.toml`
- `task test:llm-live:full` — run paid qualification against every eligible direct-
  provider model and the approved OpenRouter allowlist
- `task docs:check` — validate internal links, unique spec IDs, and done-spec acceptance state
- `task coverage` — enforce the configured runtime line-coverage threshold
- `task build` — build the locked optimized release binary (optionally for `TARGET`)
- `task smoke:interactive` — exercise the built Linux release binary through automatic
  and explicit plain/TUI selection, compatibility aliases, a pseudo-terminal, terminal
  restoration, and the unchanged one-shot command
- `task smoke:managed-process` — build the Linux release binary, kill its active owner,
  and verify a detached supervised descendant is reaped before terminal publication
- `task fix` — apply Rust formatting and Clippy fixes
- `task installers:check` — validate installer syntax, repository defaults, and checksum logic

## Live LLM qualification

The four credentialed `test:llm-live:{catalog,canary,selected,full}` tasks are
intentionally excluded from `task check`, `task test`, `task dev`, coverage, and
ordinary CI. They invoke the ignored
`llm_live::live_llm_qualification` integration test and can make authenticated network
requests. Catalog mode requires the network acknowledgement and makes no generation
requests. Canary, selected, and full runs can incur provider charges and additionally
require the cost acknowledgement:

```bash
NIB_LIVE_TESTS=1 \
NIB_LIVE_PROVIDER=openai \
task test:llm-live:catalog
```

`task test:llm-live:offline` runs only the non-ignored deterministic harness tests. It
does not read provider credentials or make live requests and is the focused gate for
matrix/schema/planner/report/workflow changes.

`NIB_LIVE_PROVIDER` accepts `openai`, `anthropic`, `google`, `grok`, `meta`,
`openrouter`, or `all`, and defaults to `all`. `NIB_LIVE_RESULTS_DIR` optionally
selects the sanitized JSON/Markdown report directory; the default is
`target/llm-live/<mode>`. Provider credentials are read only from the environment:
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GOOGLE_API_KEY`, `XAI_API_KEY`,
`META_API_KEY`, and `OPENROUTER_API_KEY`. Meta additionally requires the reviewed
`NIB_LIVE_META_BASE_URL` until a stable public catalog root is verified.

Use dedicated low-privilege test accounts with provider-side spend limits. Do not put
credentials in Task variables, command arguments, `.env` files, or repository config.
OpenRouter execution is restricted to approved entries in its exact-ID allowlist; a
full run does not expand to OpenRouter's complete catalog. Pending entries fail paid
OpenRouter modes before generation.

Selected mode reads its exact model lists and required/conditional task scenarios from
`tests/fixtures/llm_live/selected_models.toml`. The versioned file must define all six
network providers and pass review/expiry validation. Every selected report includes its
suite ID and matrix SHA-256 fingerprint. A selected OpenRouter model must also be an
approved exact entry in `openrouter_models.toml`.

For canary, selected, or full mode, also set `NIB_LIVE_ACK_COSTS=1`. Providers without complete
catalog pricing additionally require `NIB_LIVE_ALLOW_UNPRICED=1` and a provider-side
hard spend cap. Request and output ceilings can be narrowed with
`NIB_LIVE_MAX_REQUESTS` and `NIB_LIVE_MAX_OUTPUT_TOKENS`; reaching a ceiling fails the
run rather than reducing its denominator.

## Adding new tasks

When you introduce a new build, test, or automation step, add a corresponding task entry (and update any sub-Taskfiles). Document the task briefly in its `desc:` and `summary:` fields.

Reference implementations:
- `~/work/projects/skm/Taskfile.yml` (simple single-binary)
- `~/work/projects/revized/Taskfile.yml` (with includes for fe/backend/deployment)
- Central guidance in `~/work/projects/agents/docs/tech/task.md`

The root Taskfile is authoritative; update this list whenever a canonical task changes.
