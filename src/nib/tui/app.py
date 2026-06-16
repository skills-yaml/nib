"""Placeholder Textual TUI app for nib.

This will become the rich interactive experience (backlog, kanban, agent status, etc.).
"""

from rich.console import Console

console = Console()


def run_tui() -> None:
    """Entry point for the full Textual TUI."""
    console.print("[bold yellow]TUI is not yet implemented.[/]\n")
    console.print("Planned features:")
    console.print("  • Live workload / kanban board")
    console.print("  • Agent activity & delegation views")
    console.print("  • Interactive planning & review flows")
    console.print("\nFor now use the CLI: [cyan]nib --help[/]")
