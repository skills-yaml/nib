# nib Design Policy

nib uses the shared plain/TUI interaction contract described in the
[interactive specs](workspace/specs/README.md) and
[architecture](workspace/docs/architecture/architecture.md).

Preserve existing Ratatui/Crossterm components, theme configuration, keyboard
semantics, visible approval and clarification states, and plain-mode parity.
The shared interactive reducer owns behavior; renderers remain thin.
Use existing theme tokens and Unicode-width handling rather than introducing
a new token system. Test observable keyboard, cancellation and recovery behavior.

T061's question form remains an accepted development contract; this Workspace
upgrade does not implement it or change the current command surface.
