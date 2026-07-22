# nib End-User Guide

This guide covers installing, configuring, operating, updating, and removing `nib`.

## Install

The release pipeline publishes pre-built archives for Linux x86_64, macOS x86_64 and
Apple Silicon, and Windows x86_64. Runtime platform gates outside Linux were not
executed in this reconciliation. Git is required for isolated worktrees and delegation.
On Linux, the default
`hybrid` execution provider uses `bwrap` when the host permits it and otherwise runs
inside the Git worktree without OS-level isolation. Set `execution.provider = "bwrap"`
to fail closed when `bwrap` is unavailable.

### Linux and macOS

```bash
# Stable channel
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib sh

# Development channel
curl -fsSL https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.sh | \
  NIB_REPO=skills-yaml/nib NIB_CHANNEL=development sh
```

The default destination is `~/.local/bin/nib`. Set `NIB_INSTALL_DIR` to choose a
different destination. The installer downloads the matching `.sha256` asset and
verifies it before extraction.

### Windows

```powershell
& ([scriptblock]::Create((irm "https://raw.githubusercontent.com/skills-yaml/nib/main/scripts/install.ps1"))) -Channel prod -Repo skills-yaml/nib -AddToPath
```

Use `-Channel development` for development builds. The default destination is
`%USERPROFILE%\.local\bin\nib.exe`.

Terminal tools and skill hooks use POSIX shell syntax on every platform. On Windows,
install Git for Windows; `nib` finds `sh.exe` on `PATH` or inside the Git installation.
Set `NIB_POSIX_SHELL` to an alternate POSIX shell executable when Git is installed in a
nonstandard layout. `nib doctor` verifies the resolved shell before reporting success.

The `prod-latest` and `development-latest` tags are mutable rolling channels, not
version pins. The repository release workflow is their exclusive writer: no PAT,
GitHub App, reusable workflow, or manual operator may retag, edit, or delete a rolling,
staging, or backup Release while that writer is enabled. A channel moves only after the
expected archives and checksum assets have been staged and verified; interrupted
publication is reconciled by the next channel run. Emergency manual intervention must
first disable the channel workflow and reconcile its retained transaction state.

### Build From Source

Install stable Rust and [Task](https://taskfile.dev), then run:

```bash
task check
task build
./target/release/nib --help
```

## Project Setup

Run `nib` from the configured profile root, normally the Git top-level directory.
Running from a nested directory can select a different config root. Default runtime
state is project-local:

- `.nib/config.toml`: provider and runtime configuration.
- `.nib/profiles/default/sessions/*.json`: messages, plans, approvals, tool calls,
  and audit records.
- `.nib/profiles/default/memory.json`: durable environment and user memory.
- `.nib/profiles/default/daemons/`: cron state, pins, and maintenance audit.
- `.nib/skills/`: project-local skills.
- `.nib/worktrees/sessions/` and `.nib/worktrees/subagents/`: isolated Git worktrees.
- `.nib/subagents/`: linked delegation records.

Configured profiles replace `default` with their profile ID or custom `state_dir`.

The repository `.gitignore` should exclude `.nib/`; API keys and session data must not
be committed.

### Authentication

```bash
cd /path/to/project
nib auth
```

The wizard writes the selected provider, model, and API key to `.nib/config.toml`.
Config writes use owner-only permissions on Unix, but credentials remain plaintext.
The configured adapters are OpenAI, Anthropic, Google Gemini, xAI Grok, OpenRouter,
Meta, and the deterministic Mock provider used for tests. Credentials can instead
come from `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GOOGLE_API_KEY`, `XAI_API_KEY`,
`OPENROUTER_API_KEY`, or `META_API_KEY`.

### Configuration

```bash
nib config show
nib config show --show-secrets
nib config validate
nib config edit
```

`config show` redacts stored credentials by default. Treat `--show-secrets` output as
sensitive. `config edit` uses `$VISUAL`, then `$EDITOR`; if editing or validation
fails, nib restores the previous valid configuration.

The supported schema is shown below. Runtime defaults are shown for operational
sections; the OpenAI provider and named workspace profile are examples:

```toml
[llm]
active_provider = "openai"
context_length = 128000

[llm.providers.openai]
model = "gpt-4o"
api_key = "replace-or-use-OPENAI_API_KEY"
api_keys = []
# base_url = "https://api.openai.com/v1/chat/completions"

[agent]
max_turns = 90
tool_use_enforcement = true

[terminal]
backend = "local"
timeout = 180

[execution]
provider = "hybrid"       # internal | hybrid | bwrap
default_profile = "restricted" # restricted | internal
plan_mode = true

[execution.boundaries]
allow_write = []
network = "restricted"    # restricted | enabled | disabled

[execution.boundary_profiles.offline]
allow_write = []
network = "disabled"

[approvals]
mode = "manual"           # manual | smart | policy | off

[compression]
enabled = true
threshold = 0.50
target_ratio = 0.20

[memory]
enabled = true
provider = "built-in"     # built-in | json

[workload]
enabled = true
store = "sessions"        # sessions | json
require_reconciliation = true

[skills]
enabled = true
paths = [".nib/skills"]

[mcp]
client_enabled = true
server_enabled = true

[daemons]
cron_enabled = true
curator_enabled = true
retention_days = 30
interval_seconds = 86400
allow_destructive_cleanup = false

[profiles]
default = "workspace"

[[profiles.active]]
id = "workspace"
root = "."
env_file = ".env.nib"
active_skills = []
skill_paths = [".nib/skills"]
state_dir = ".nib/profiles/workspace"

[mcp.servers.example]
command = "path/to/pinned-mcp-server"
args = ["--stdio"]
cwd = "."
request_timeout_secs = 30

[mcp.servers.example.env]
MODE = "production"
```

`terminal.backend` is `local` in this release. `profiles.default` selects a workspace
profile; `execution.default_profile` selects the shell sandbox profile. Boundary
network settings apply only to sandboxed terminal processes, not LLM HTTP, web tools,
skill installation, or MCP child-process startup.

Named boundary profiles are optional tighten-only overlays. An instruction line such
as `nib-boundary: profile offline` selects the matching
`execution.boundary_profiles.offline` entry for agent-selected execution. Its network
policy must be equal or stricter than `execution.boundaries`, and its writable paths
must be a subset. Invalid, unknown, conflicting, or weaker selections fail closed
before approval and worktree creation. A named profile upgrades direct `internal`
execution to at least `hybrid`; a disabled-network profile requires `bwrap`.

The main sections are:

- `llm` and `llm.providers`: active provider, model, context length, and credentials.
- `agent`: turn bound and tool-use enforcement.
- `terminal`: backend and default command timeout.
- `execution`: sandbox provider, profile, plan gate, and writable boundaries.
- `approvals`: `manual`, `smart`, `policy`, or `off` mode.
- `compression` and `memory`: long-session context and durable facts.
- `daemons`: scheduler and curator policy.
- `skills`, `mcp`, `profiles`, and `workload`: ecosystem and persistence settings.

Run `nib doctor` after manual configuration changes. It validates local readiness and
credential presence, not live provider connectivity.

## Use

### One-Shot Task

```bash
nib run "Refactor the parser and run the canonical checks"
```

Useful options include `--session <id>` to resume, `--provider <name>`, `--mode plan`,
`--model <name>`, and `--max-steps <count>`. Omitting `--max-steps` uses
`agent.max_turns`. Normal execution creates and persists a structured plan before
mutating tools are allowed. Default manual mode prompts for the plan and calls not
auto-classified as safe. `--yes` bypasses interactive plan and tool approval; use it
only in an already trusted environment. Explicit deny policies still take precedence.
When the agent calls `ask_question`, the CLI prints the available options and accepts
either an option number or free-form text on the same input stream. Closed or empty
question input stops the run and reconciles the session without continuing execution.

In a normal Git checkout, edits remain in a `nib/session/*` branch under
`.nib/worktrees/sessions/` until reviewed and merged manually. When nib is already
running inside a linked worktree, edits remain in that worktree.

### Interactive Chat

```bash
nib chat
nib chat --session <id>
nib chat --auth
```

Chat commands:

- `/model` or `/model <name>` lists or selects a model.
- `/providers` lists configured providers.
- `/session` prints the active session; `/clear` switches to a new session.
- `/skills list|install|remove` manages skills.
- `/mcp list|add|remove` manages MCP servers.
- `/help` prints commands; `/quit`, `/exit`, or `/q` exits.

Agent questions share chat's input stream: answer with the displayed option number or
typed text, then continue entering chat commands after the agent turn completes.

### TUI

```bash
nib tui
nib tui --run "Inspect the failing tests and fix them"
```

The second form starts an agent run in the background. Streamed model output and tool
lifecycle events appear live. Calls that still require interactive approval open a
modal showing the tool and arguments; press `Y` to approve, `N` or `Esc` to deny.
`ask_question` opens a selectable or typed-response modal and resumes the same loop.

### Skills

A skill is a directory containing a `SKILL.md` with YAML frontmatter. Skills can be
project-local under `.nib/skills/` or installed globally under
`~/.config/nib/skills/`.

```bash
nib skill list
nib skill install ./path/to/skill
nib skill install ./path/to/SKILL.md
nib skill install https://github.com/example/skill.git
nib skill remove skill-name
```

Install and remove operate on the global directory; set `NIB_SKILLS_DIR` to override
it. Discovery also checks `.grok/skills`, `.agents/skills`, configured `skills.paths`,
and profile `skill_paths`. A non-empty profile `active_skills` list selects those
skills explicitly instead of tag matching.

`nib skill list` is strict: it fails with a contextual error when a discovered
manifest is malformed or the bounded scan cannot prove that the inventory is complete.

Skill tags select relevant instructions. Structured constraints can deny a tool or
force approval for commands; post-tool hooks remain subject to the same executor and
approval policy. Installation publishes only `SKILL.md` and its declared references
and assets; resource count, path depth, per-file bytes, and aggregate bytes are
bounded. Git sources use a time-bounded, noninteractive partial checkout of those
declared paths. Remote skills are not signed or checksummed, and installation remains
an explicit CLI action outside the tool sandbox. Review the source, `SKILL.md`,
constraints, and hooks before installation.

### MCP

Configure outbound MCP servers with:

```bash
nib mcp add example /path/to/pinned-mcp-server -- --stdio
nib mcp list
nib mcp remove example
```

Discovered tools are namespaced as `server::tool` and still pass through nib's
approval and audit path. To expose nib to another MCP client:

```bash
nib mcp-server
```

The stdio server advertises the agent entrypoints and gated core tools. Configure the
client to launch `nib mcp-server` with its working directory set to the target project.
Outbound MCP currently supports stdio child processes. Starting a configured server
command is not sandboxed, so review and pin the executable and package; only its
advertised tool invocations enter nib's approval and audit path. Configure `env`,
`cwd`, and `request_timeout_secs` directly in TOML when required.

Timeout, cancellation, manager drop, fatal transport, and direct-server-exit cleanup
are bounded, terminate descendants that remain in the managed process group, and reap
the direct child. During a supervised subagent run, MCP children also remain inside the
bwrap PID namespace rooted at the validated namespace PID 1 on Linux. Native Job Object
and process-group mechanism tests exist for Windows and macOS, but production subagent
delegation fails closed on those
platforms until managed workers cannot forge the durable cleanup authority. Linux
locally proves cleanup of a descendant that calls `setsid`.

HTTP/SSE MCP transports and OAuth are not implemented in this release. Both outbound
and inbound v1 MCP support use stdio.

Inbound stdio cannot safely reuse stdin for an approval prompt. Calls that require an
interactive decision therefore fail closed unless an explicit allow policy or
configuration already covers them.

Inbound cancellation, stdin disconnect, fatal input, and blocked-stdout shutdown join
active local requests and reconcile their audit state. Cancellation audit lock waits have
an absolute deadline; a stuck live holder produces a surfaced internal error or nonzero
server exit after active process groups have been stopped. Windows terminal and agent
descendant containment remains pending runtime validation.

### External Messaging Gateways

nib does not host Telegram, Slack, or Discord authentication, webhook/socket
listeners, or reply delivery. An external adapter owns those provider-specific
concerns and sends a normalized payload to nib's gateway. The gateway selects a
profile-scoped session from stable source identifiers; callers cannot inject tools or
bypass `ToolExecutor` policy.

### Other Commands

```bash
nib --version
nib version
nib context . --task "inspect the parser"
nib task list
nib task get <task-id>
nib task cancel <task-id>
nib task reconcile
```

`nib context` prints assembled AGENTS and skill context. `demo-tool` is a developer
diagnostic rather than a normal agent workflow. Background terminal calls and
scheduled wakes create profile-scoped durable task records. The task commands emit
JSON for inspecting them, requesting cancellation, and failing workers whose leases
have expired. Reconciliation never replays a command with unknown side effects.

The agent can use `manage_memory` to list, read, persist, or remove profile-scoped
`environment` and `user` facts. Reads are non-mutating. Writes require interactive
approval or an explicit allow policy, and deletes are classified as destructive.
Memory is stored in the selected profile's `memory.json` and is included in bounded
runtime context on later sessions.

## Safety and Persistence

Agent-selected tool calls use `ToolExecutor`. It applies scope checks, tool and shell
classification, active AGENTS.md and skill policy, approval, worktree isolation,
optional `bwrap`, redaction, and a session audit record. User-invoked skill installers
and configured MCP child-process startup are separate trust boundaries described
above.

Mutating tools use a session worktree. For shell commands, `internal` runs directly in
that worktree, `hybrid` uses Linux `bwrap` when usable and otherwise runs directly in
the worktree, and `bwrap` fails closed when unavailable. Only usable `bwrap` provides
OS-level filesystem and network isolation. `nib doctor` reports the effective
capability.

Managed Git commands disable hooks and major ambient configuration sources and reject
detected executable helpers. Git still reads mutable repository and per-worktree config
after nib's helper preflight. A malicious peer already running as the same OS user is
outside nib's isolation boundary; deployments requiring that threat model need a
separate account, VM/container, or privileged broker.

Cleanup removes only paths, registrations, and refs backed by exact ownership evidence.
Ambiguous or substituted artifacts are preserved and reported for inspection. In
particular, a failed `git worktree add` can leave an unproven registration that nib will
not delete. Versioned worktree ownership receipts persist intent, generation, staged
path/ref provenance, registration and filesystem identities, and cleanup phases so
restart can resume only exact owned work; completed receipts are compacted under a
bounded store policy.

Foreground subagents run in a hidden worker owned by an independent supervisor. The
interactive owner holds a lifetime pipe; EOF, explicit cancellation, and normal worker
completion all drive bounded scope cleanup. The launcher records the exact supervisor
identity before writing request bytes. A terminal subagent record is published only
after generation-fenced authority proves either that a Running descendant scope is
empty or that a Prepared launch never released its workload gate. The latter is a
distinct launch-abort proof with `workload_never_launched=true`; it does not claim
descendant cleanup.

Linux requires a usable bwrap PID namespace for subagents and fails closed without it.
The supervisor first runs the exact bwrap gate and pidfd cleanup capability probe. Each
launch then waits for an EOF-sensitive PID-1 command gate, validates and durably records
that namespace identity, and only then publishes Running and releases the worker. A
supervisor crash before bwrap, after bwrap but before PID-1 discovery, or after PID-1
readiness but before Running leaves the gate closed. Recovery waits for the recorded
supervisor to disappear and exact-signals a recorded namespace PID 1 through pidfd when
needed before publishing launch-abort authority. Normal or restart cleanup after
Running signals only the recorded namespace generation through pidfd. Unix child
launchers retain process-group authority only for groups they created and observe the
leader with `waitid(..., WNOWAIT)` before signalling lingering members and reaping it.
Windows and macOS production delegation also fail closed before worktree or ownership
state is created. Their native Job Object and group-contained tests are backend evidence,
not an enabled production contract. `nib doctor` reports production supervised-subagent
availability separately from direct terminal fallback. Durable background terminal
jobs, schedules, and nested subagents are rejected inside a foreground scope; top-level
durable tasks retain their separate persisted worker ownership.

Session compression summarizes old context for the model without deleting the raw
message audit trail. At agent-run startup, durable cron state determines whether
profile maintenance is due. The curator can conditionally detach stale sessions,
memory entries, and profile-managed skill caches from the authoritative namespace only
when destructive cleanup is enabled; it respects pins and records every maintenance
decision. Ambiguous or raced physical cleanup is preserved and reported rather than
claimed as exact deletion. Detached task workers use persisted, revocable leases; a
reconciler fences an expired worker before recording failure so a late process cannot
overwrite the authoritative terminal state.

## Diagnose and Maintain

```bash
nib doctor
```

Doctor validates configuration, credential presence, Git/worktrees, MCP commands,
skills, profile paths, persistence, daemons, the command shell, and sandbox capability.
It creates missing profile directories and temporary write probes, but does not call
provider APIs or run destructive cleanup. It exits nonzero for required checks that
fail.

To update, rerun the installer for the same channel. `prod-latest` is the stable rolling
release; `development-latest` is the prerelease channel. The installers verify the
matching `.sha256` asset before extraction. Do not manually retag or edit a rolling
GitHub Release while its publication workflow is active.

## Uninstall

Remove the installed binary from the configured install directory:

```bash
rm ~/.local/bin/nib
```

```powershell
Remove-Item "$env:USERPROFILE\.local\bin\nib.exe" -Force
```

Project `.nib/` directories contain configuration, credentials, sessions, worktrees,
branches, and memory. Do not delete them while Git still registers nib worktrees.
Failed creation or cleanup may intentionally preserve an unproven registration, path,
or ref. Inspect the porcelain listing and remove only entries you have verified, then
review the associated branches:

```bash
git worktree list --porcelain
git worktree remove /path/from/the/list
git branch --list 'nib/session/*' 'nib/subagent/*'
git worktree prune
rm -rf /path/to/project/.nib
```

Delete branches only after reviewing or merging their changes. Global skills are
stored in `NIB_SKILLS_DIR` or `~/.config/nib/skills/` and can be removed separately.
On Windows, remove the user `PATH` entry if installation used `-AddToPath`.
