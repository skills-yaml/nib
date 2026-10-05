# nib Gate Modules

All repeatable operations use Task. Final verification runs task verify, which
runs task check and task test once. No receipt-based reuse is enabled; missing
or unknown freshness always requires a new complete run. Modules below guide
iteration; they never replace full final coverage.

| Module | Task command | Inputs | Dependencies and consumers | Resources | Pass condition | Freshness |
| --- | --- | --- | --- | --- | --- | --- |
| Workspace governance | task workspace:check | AGENTS.md, DESIGN.md, workspace tree, Cargo.toml, native validator sources | Specs, instructions, memory, versions and WIP records | Serial Cargo access; filesystem only | Every native structure/catalog/memory/version/coordination check succeeds | Rerun on any named input change; no reuse |
| Documentation | task docs:check | Current Markdown and docs_integrity.rs | Moved links, lifecycle and Workspace governance | Serial Cargo access; filesystem only | Native governance and all integrity tests pass | Rerun after path/link/spec changes |
| Validator fixtures | task test:workspace | workspace_governance.rs and shared validator modules | Gate semantics and downstream completion claims | Temporary isolated fixture directories; serial Cargo access | Valid examples pass; malformed metadata, collisions and stale versions fail | Rerun after validator changes |
| Agent context | task test:agent-context | src/context/, src/agent/, session fixtures, changed instruction inputs | Runtime discovery, planning, compression, context budgets | Serial tests; credential-free fixtures | Bounded current and legacy context behavior passes | Rerun for changed roots, budgets, prompts or consumers |
| Installer/release contracts | task test:installers | scripts/, release workflows, installers.rs | Release filtering, qualification and transaction authority | Serial process fixtures; no live publication | Complete installer and release-transaction fixtures pass | Rerun on workflow/script/contract changes |
| Task composition | task test:task-contract | Taskfile.yml and installer task-contract fixtures | Fast-check separation and canonical verification | Serial Cargo access | Static gate contains no hidden full test run; verify invokes check/test once | Rerun on Task definitions |
| Rust static | task check | Cargo manifests/lock, Rust sources, Task gates | All local targets/features | Shared Cargo build directory; serialize | Installer syntax, governance, formatting and warning-denying Clippy pass | Complete rerun on final candidate |
| Full tests | task test | All source, tests, fixtures, workflow contracts | All runtime and development consumers | Serial suite; local sockets/process fixtures; no live providers | Every non-ignored test and doctest succeeds | Complete rerun on final candidate |

Run the narrowest affected module and its transitive consumers during coherent
changes. An instruction change selects governance, docs and agent context;
a release-filter change also selects installer contracts; validator/Task changes
select their fixture modules. Unknown or cross-cutting scope selects task verify.
Positive and negative native fixture cases validate gate behavior. No optimistic
path-only selector or automatically reused success receipt is introduced.
