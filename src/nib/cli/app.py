"""nib CLI (Typer + Rich)."""

from __future__ import annotations

import typer
from rich.console import Console
from rich.panel import Panel

from nib import __version__

app = typer.Typer(
    name="nib",
    help="AI agent for coding and workload management. Owns your backlog and ships.",
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
    """nib - the persistent coding + workload agent.

    Run without a subcommand to launch the interactive TUI.
    """
    if ctx.invoked_subcommand is None:
        console.print(
            Panel.fit(
                "[bold]nib[/] — AI agent for coding and workload management\n\n"
                "Launch the full TUI with [cyan]nib tui[/] (or just [cyan]nib[/] in the future).\n"
                "See [cyan]nib --help[/] for commands.",
                title="Welcome",
                border_style="blue",
            )
        )
        console.print("Use [bold]nib version[/] or start building real commands.")


@app.command()
def tui() -> None:
    """Launch the interactive Textual TUI (workload dashboard, kanban, etc.)."""
    console.print("[yellow]TUI coming soon.[/yellow] Launching a placeholder for now...")
    # Later: from nib.tui.app import run_tui; run_tui()


@app.command()
def context(
    path: str = typer.Argument(".", help="Project directory to load context for"),
    task: str = typer.Option(
        None, "--task", "-t", help="Optional task description for skill activation"
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
        from nib.core.workload import WorkloadStore

        ws = WorkloadStore()
        await ws.init_db()
        executor = default_executor
        executor.workload_store = ws
        executor.project_root = project
        result = await executor.execute(call, current_task_id="demo-task-001")
        console.print("[bold green]Tool Result:[/]", result.model_dump())
        history = await ws.get_tool_history_for_task("demo-task-001")
        console.print(f"[dim]Tool history for demo task: {len(history)} records[/dim]")

    asyncio.run(_run())
