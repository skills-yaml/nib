"""nib CLI (Typer + Rich)."""

from __future__ import annotations

import typer
from rich.console import Console
from rich.panel import Panel

from nib import __version__

app = typer.Typer(
    name="nib",
    help="AI agent for coding. Sessions and tool history stored locally in .nib/",
    add_completion=False,
    rich_markup_mode="rich",
)
console = Console()


@app.command()
def version() -> None:
    """Show the installed version."""
    console.print(f"[bold]nib[/] [cyan]{__version__}[/]")


@app.callback(invoke_without_command=True)
def main(ctx: typer.Context) -> None:
    """nib - interactive AI coding agent.

    Start chatting with: nib chat
    Run a one-shot task:  nib run "your goal"
    """
    if ctx.invoked_subcommand is None:
        console.print(
            Panel.fit(
                "[bold]nib[/] — AI coding agent\n\n"
                "Start interactive chat:  [cyan]nib chat[/]\n"
                "One-shot goal:         [cyan]nib run \"your goal\"[/]\n"
                "See providers & auth:  [cyan]nib chat[/] then /auth or /providers\n"
                "Help:                  [cyan]nib --help[/]",
                title="Welcome",
                border_style="blue",
            )
        )


@app.command()
def tui() -> None:
    """Launch the interactive Textual TUI (sessions, history in .nib/)."""
    console.print("[yellow]TUI coming soon.[/yellow] Launching a placeholder for now...")
    # Later: from nib.tui.app import run_tui; run_tui()


@app.command()
def context(
    path: str = typer.Argument(".", help="Project directory to load context for"),
    task: str = typer.Option(
        None, "--task", "-t", help="Optional description for skill activation"
    ),
) -> None:
    """Assemble and display rich project context (AGENTS.md + skills + ...)."""
    from pathlib import Path

    from nib.context.loader import assemble_context, format_context_for_prompt

    project_path = Path(path).resolve()
    ctx = assemble_context(project_path=project_path, task_description=task)

    console.print(f"[bold]Context for[/] {project_path}")
    if ctx.get("agents_md"):
        console.print("\n[green]AGENTS.md loaded[/]")
    if skills := ctx.get("active_skills"):
        console.print(f"[green]{len(skills)} skills activated[/]")
    console.print("\n--- Formatted for prompt ---")
    console.print(format_context_for_prompt(ctx)[:2000])


@app.command("demo-tool")
def demo_tool(
    tool: str = typer.Argument(
        "read_file", help="Tool to demo (read_file, grep, run_terminal, etc.)"
    ),
    arg: str = typer.Option("README.md", "--arg", "-a", help="Simple argument value for the tool"),
) -> None:
    """Quick demo of the new ToolExecutor + permission system (for development)."""
    import asyncio
    from pathlib import Path

    from nib.tools.executor import default_executor
    from nib.tools.models import ToolCall

    project = Path(".").resolve()

    call = ToolCall(
        tool_name=tool,
        arguments={"path": arg, "command": arg} if tool == "run_terminal" else {"path": arg},
        project_root=str(project),
    )

    async def _run():
        from nib.core.workload import SessionStore
        from pathlib import Path

        project = Path(".").resolve()
        ss = SessionStore(project_root=project)
        session = ss.create_session()

        executor = default_executor
        executor.session_store = ss
        executor.project_root = project
        result = await executor.execute(call, session_id=session.id)
        console.print("[bold green]Tool Result:[/]", result.model_dump())

        # Show recent tool calls from the session file
        loaded = ss.load_session(session.id)
        print(f"[dim]Tool calls recorded in session: {len(loaded.tool_calls) if loaded else 0}[/dim]")

    asyncio.run(_run())


@app.command("run")
def run(
    goal: str = typer.Argument(..., help="High-level goal for the agent to achieve"),
    session: str = typer.Option(None, "--session", "-s", help="Existing session ID to resume"),
    steps: int = typer.Option(15, "--max-steps", help="Maximum reasoning steps"),
    mode: str = typer.Option("execute", "--mode", help="plan or execute"),
    provider: str = typer.Option("mock", "--provider", "-p", help="openai, anthropic, google, grok, openrouter, mock"),
    model: str | None = typer.Option(None, "--model", "-m", help="Model name for the provider (e.g. gpt-4o for openai, grok-2 for grok)"),
    api_key: str | None = typer.Option(None, "--api-key", help="API key (falls back to env vars)"),
    base_url: str | None = typer.Option(None, "--base-url", help="Custom base URL"),
) -> None:
    """Run the LLM-driven agent loop for a goal.

    All conversation turns and tool calls are persisted to .nib/sessions/<id>.json
    in the current project.

    Examples:
        nib run "refactor the auth module" -p grok -m grok-2
        nib run "add tests" -p openrouter -m anthropic/claude-3.5-sonnet   # model name relative to provider
        nib run "explore" -p openai -m gpt-4o
    """
    import asyncio
    from pathlib import Path

    from nib.agent.loop import run_agent_loop
    from nib.core.workload import SessionStore
    from nib.llm import get_llm_client
    from nib.tools.executor import default_executor

    project = Path(".").resolve()
    ss = SessionStore(project_root=project)

    if session:
        sid = session
    else:
        s = ss.create_session()
        sid = s.id

    # Get LLM client for the chosen provider
    llm = get_llm_client(
        provider=provider,
        model=model,
        api_key=api_key,
        base_url=base_url,
    )

    executor = default_executor
    executor.session_store = ss
    executor.project_root = project

    console.print(f"[bold]Starting agent loop[/] session={sid} mode={mode} provider={provider}")
    if model:
        console.print(f"Model: {model}")
    console.print(f"Goal: {goal}")

    async def _run():
        summary = await run_agent_loop(
            session_store=ss,
            session_id=sid,
            goal=goal,
            llm=llm,
            executor=executor,
            max_steps=steps,
            mode=mode,
            project_root=project,
        )
        console.print("[green]Loop finished[/]")
        console.print(summary)

        loaded = ss.load_session(sid)
        if loaded:
            console.print(f"[dim]Messages in session: {len(loaded.messages)}[/dim]")
            console.print(f"[dim]Tool calls recorded: {len(loaded.tool_calls)}[/dim]")

    asyncio.run(_run())


@app.command("auth")
def auth_cmd() -> None:
    """Run the interactive wizard to configure LLM providers.
    Select a provider and provide its API key. You can add multiple providers.
    """
    from pathlib import Path
    from rich.console import Console as RichConsole
    from rich.prompt import Prompt, Confirm

    from nib.config import LLMConfig, SUPPORTED_PROVIDERS

    rich_console = RichConsole()
    project = Path(".").resolve()
    llm_config = LLMConfig(project_root=project)

    def run_auth_flow():
        rich_console.print("\n[bold]LLM Provider Setup Wizard[/bold]")
        rich_console.print("Select a provider:")

        providers_list = list(SUPPORTED_PROVIDERS.items())
        for i, (p, desc) in enumerate(providers_list, 1):
            rich_console.print(f"  {i}. {p} - {desc}")

        choice = Prompt.ask("Enter number or name", default="grok")
        if choice.isdigit():
            idx = int(choice) - 1
            if 0 <= idx < len(providers_list):
                provider = providers_list[idx][0]
            else:
                provider = "grok"
        else:
            provider = choice.lower().strip()

        if provider not in SUPPORTED_PROVIDERS and provider != "mock":
            rich_console.print(f"[yellow]Unknown, defaulting to grok[/yellow]")
            provider = "grok"

        default_models = {
            "openai": "gpt-4o",
            "anthropic": "claude-3-5-sonnet-20241022",
            "google": "gemini-1.5-pro",
            "grok": "grok-2",
            "openrouter": "openrouter/anthropic/claude-3.5-sonnet",
        }
        default_model = default_models.get(provider, "gpt-4o")

        model = Prompt.ask("Default model", default=default_model)
        api_key = Prompt.ask(f"API key for {provider} (hidden)", password=True, default="")

        llm_config.add_or_update_provider(
            provider=provider,
            model=model,
            api_key=api_key or None,
        )
        rich_console.print(f"[green]Configured {provider} with model {model}[/green]")

    if llm_config.list_providers():
        rich_console.print("Existing providers found. You can add more.")

    run_auth_flow()
    while Confirm.ask("Add another provider?", default=False):
        run_auth_flow()

    rich_console.print("[green]Done. Providers saved to .nib/config.json[/green]")
    rich_console.print("Start chat with: nib chat")


@app.command("chat")
def chat(
    session: str | None = typer.Option(None, "--session", "-s", help="Resume a specific session ID"),
    auth: bool = typer.Option(False, "--auth", help="Run the provider auth wizard before starting chat"),
) -> None:
    """Start an interactive chat with nib.

    - Use --auth to run the setup wizard: select provider(s) and enter API key(s).
    - In chat, type /model (with no arg to list available models for the current provider and select by number,
      or provide a name directly if correct).
    - You can only switch models for the currently active provider.
    - All history + tool calls saved to .nib/sessions/
    """
    import asyncio
    from pathlib import Path

    from rich.prompt import Prompt, Confirm
    from rich.console import Console as RichConsole

    from nib.config import LLMConfig, SUPPORTED_PROVIDERS
    from nib.core.workload import SessionStore
    from nib.llm import get_llm_client
    from nib.tools.executor import default_executor
    from nib.agent.loop import run_agent_loop

    rich_console = RichConsole()
    project = Path(".").resolve()
    llm_config = LLMConfig(project_root=project)
    session_store = SessionStore(project_root=project)

    # --- Auth / Provider Setup Flow ---
    def run_auth_flow():
        rich_console.print("\n[bold]LLM Provider Setup[/bold]")
        rich_console.print("Select a provider to configure (you can add more later):")

        providers_list = list(SUPPORTED_PROVIDERS.items())
        for i, (p, desc) in enumerate(providers_list, 1):
            rich_console.print(f"  {i}. {p} - {desc}")

        choice = Prompt.ask("Enter number or provider name", default="grok")
        if choice.isdigit():
            idx = int(choice) - 1
            if 0 <= idx < len(providers_list):
                provider = providers_list[idx][0]
            else:
                provider = "grok"
        else:
            provider = choice.lower().strip()

        if provider not in SUPPORTED_PROVIDERS and provider != "mock":
            rich_console.print(f"[yellow]Unknown provider '{provider}', defaulting to grok[/yellow]")
            provider = "grok"

        default_models = {
            "openai": "gpt-4o",
            "anthropic": "claude-3-5-sonnet-20241022",
            "google": "gemini-1.5-pro",
            "grok": "grok-2",
            "openrouter": "openrouter/anthropic/claude-3.5-sonnet",
        }
        default_model = default_models.get(provider, "gpt-4o")

        model = Prompt.ask("Default model", default=default_model)
        api_key = Prompt.ask(f"API key for {provider} (input hidden)", password=True, default="")

        llm_config.add_or_update_provider(
            provider=provider,
            model=model,
            api_key=api_key or None,
        )
        rich_console.print(f"[green]Saved provider: {provider} (model: {model})[/green]")

    if auth or not llm_config.list_providers():
        run_auth_flow()

    # Allow adding multiple providers easily
    while Confirm.ask("Add another LLM provider?", default=False):
        run_auth_flow()

    # If multiple, let user choose the active one
    provs = llm_config.list_providers()
    if len(provs) > 1:
        rich_console.print("\nMultiple providers configured. Choose the active one for this chat:")
        prov_list = list(provs.keys())
        for i, p in enumerate(prov_list, 1):
            rich_console.print(f"  {i}. {p}")
        choice = Prompt.ask("Number or name", default=prov_list[0])
        if choice.isdigit():
            idx = int(choice) - 1
            if 0 <= idx < len(prov_list):
                llm_config.set_active(prov_list[idx])
            else:
                llm_config.set_active(prov_list[0])
        else:
            if choice in prov_list:
                llm_config.set_active(choice)
            else:
                llm_config.set_active(prov_list[0])

    # Ensure we have an active provider
    current_provider = llm_config.active_provider
    current_cfg = llm_config.get_provider(current_provider)
    if not current_cfg and current_provider != "mock":
        rich_console.print("[yellow]No active provider configured. Running wizard...[/yellow]")
        run_auth_flow()
        current_provider = llm_config.active_provider
        current_cfg = llm_config.get_provider(current_provider)

    # Create LLM client
    llm = llm_config.get_current_client() if current_provider != "mock" else get_llm_client("mock")

    # Session setup
    if session:
        sid = session
        loaded_sess = session_store.load_session(sid)
        if loaded_sess:
            rich_console.print(f"[dim]Resumed session {sid}[/dim]")
        else:
            rich_console.print(f"[yellow]Session {sid} not found, creating new one.[/yellow]")
            s = session_store.create_session()
            sid = s.id
    else:
        s = session_store.create_session()
        sid = s.id

    executor = default_executor
    executor.session_store = session_store
    executor.project_root = project

    rich_console.print(
        f"\n[bold green]nib chat[/bold green]  |  session: {sid}  |  active provider: {current_provider}"
    )
    rich_console.print("[dim]Type your message. Use /model (no arg to list & select, or /model name). /help for commands. Ctrl+C to exit.[/dim]\n")

    # Show recent history
    loaded = session_store.load_session(sid)
    if loaded and loaded.messages:
        for msg in loaded.messages[-6:]:  # show last few
            role_color = "cyan" if msg.role == "user" else "green"
            rich_console.print(f"[{role_color}]{msg.role}[/]: {msg.content[:200]}")

    # --- Interactive Loop ---
    while True:
        try:
            user_input = Prompt.ask("\n[bold cyan]You[/bold cyan]")
            if not user_input.strip():
                continue

            # Command handling for switching
            if user_input.startswith("/"):
                cmd = user_input[1:].strip().lower()
                parts = cmd.split(maxsplit=1)
                command = parts[0]
                arg = parts[1] if len(parts) > 1 else None

                if command in ("quit", "exit", "q"):
                    rich_console.print("[dim]Goodbye.[/dim]")
                    break
                elif command == "help":
                    rich_console.print("""
[dim]Commands (during chat - only model switching allowed):
  /model                List available models for current provider and select (or type name directly)
  /model <name>         Directly set model (must be valid for current provider)
                        Model always belongs to the active provider.
  /providers            List configured providers
  /session              Show current session ID
  /clear                Start a fresh session (new history)
  /help                 This help
  /quit, /exit, /q      Exit chat

Provider selection and keys are done via the auth wizard (run with `nib chat --auth`).
You can configure multiple providers in the wizard.
[/dim]
""")
                elif command == "providers":
                    provs = llm_config.list_providers()
                    rich_console.print("[bold]Configured providers:[/bold]")
                    for name, cfg in provs.items():
                        active = " (active)" if name == llm_config.active_provider else ""
                        rich_console.print(f"  - {name}: {cfg.model}{active}")
                    if not provs:
                        rich_console.print("  (none)")
                elif command == "model":
                    cfg = llm_config.get_provider()
                    provider_name = cfg.provider if cfg else llm_config.active_provider
                    available = llm_config.get_available_models(provider_name)

                    if not arg:
                        # List available models for the current provider and allow selection
                        new_model = None
                        if not available:
                            rich_console.print(f"[yellow]No predefined models for {provider_name}. Enter full model name.[/yellow]")
                            new_model = Prompt.ask("Model name")
                        else:
                            rich_console.print(f"\n[bold]Available models for {provider_name}:[/bold]")
                            for i, m in enumerate(available, 1):
                                current_marker = " (current)" if m == (cfg.model if cfg else "") else ""
                                rich_console.print(f"  {i}. {m}{current_marker}")

                            rich_console.print("\nEnter number to select, or type the exact model name.")
                            choice = Prompt.ask("Selection")

                            if choice.isdigit():
                                idx = int(choice) - 1
                                if 0 <= idx < len(available):
                                    new_model = available[idx]
                                else:
                                    rich_console.print("[red]Invalid number.[/red]")
                                    continue
                            else:
                                new_model = choice.strip()

                        if not new_model:
                            continue

                        # Validate
                        is_valid = (new_model in available) or (provider_name == "openrouter" and new_model.startswith("openrouter/")) or not available
                        if not is_valid:
                            rich_console.print(f"[red]Model '{new_model}' not valid for {provider_name}.[/red]")
                            continue

                        llm_config.update_model_for_active(new_model)
                        llm = llm_config.get_current_client()
                        rich_console.print(f"[green]Switched model to '{new_model}' for provider '{provider_name}'[/green]")
                    else:
                        # Direct name provided
                        new_model = arg.strip()
                        is_valid = (new_model in available) or (provider_name == "openrouter" and new_model.startswith("openrouter/")) or not available

                        if not is_valid:
                            rich_console.print(f"[red]Invalid model '{new_model}' for provider '{provider_name}'.[/red]")
                            if available:
                                rich_console.print("Available: " + ", ".join(available[:5]) + ("..." if len(available) > 5 else ""))
                            continue

                        llm_config.update_model_for_active(new_model)
                        llm = llm_config.get_current_client()
                        rich_console.print(f"[green]Set model to '{new_model}' for provider '{provider_name}'[/green]")
                elif command == "session":
                    rich_console.print(f"Current session: [bold]{sid}[/bold]")
                elif command == "clear":
                    new_s = session_store.create_session()
                    sid = new_s.id
                    rich_console.print(f"[yellow]Started fresh session {sid}[/yellow]")
                else:
                    rich_console.print(f"[red]Unknown command: {command}. Only /model is supported for changes. Try /help[/red]")
                continue

            # Normal message
            session_store.append_message(sid, "user", user_input)

            # Run one step of the agent loop with current LLM
            try:
                summary = asyncio.run(
                    run_agent_loop(
                        session_store=session_store,
                        session_id=sid,
                        goal=user_input,
                        llm=llm,
                        executor=executor,
                        max_steps=6,  # keep short for interactive feel
                        mode="execute",
                        project_root=project,
                    )
                )
                # The loop already appended messages and tool results
                loaded = session_store.load_session(sid)
                if loaded and loaded.messages:
                    last = loaded.messages[-1]
                    if last.role == "assistant":
                        rich_console.print(f"[green]Assistant[/green]: {last.content}")
            except Exception as e:
                rich_console.print(f"[red]Error during generation:[/red] {e}")
                session_store.append_message(sid, "assistant", f"[error] {str(e)}")

        except (KeyboardInterrupt, EOFError):
            rich_console.print("\n[dim]Chat ended. Session saved to .nib/sessions/[/dim]")
            break
