//! TUI internals split for T043 module size.

use super::*;

pub(crate) fn restore_terminal_to(
    output: &mut impl io::Write,
    restore_raw_mode: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // Attempt all restorations even if an earlier one fails. In particular,
    // mouse capture and bracketed paste must not remain enabled on a raw-mode
    // or alternate-screen error.
    let mouse_result = execute!(output, DisableMouseCapture);
    let paste_result = execute!(output, DisableBracketedPaste);
    let alternate_result = execute!(output, LeaveAlternateScreen);
    // Windows mouse cleanup restores the input mode it saved after raw mode
    // was enabled. Disable raw mode last so that snapshot cannot re-enable it.
    let raw_result = restore_raw_mode();
    let mut errors = Vec::new();
    if let Err(error) = raw_result {
        errors.push(format!("failed to disable raw mode: {error}"));
    }
    if let Err(error) = mouse_result {
        errors.push(format!("failed to disable mouse capture: {error}"));
    }
    if let Err(error) = paste_result {
        errors.push(format!("failed to disable bracketed paste: {error}"));
    }
    if let Err(error) = alternate_result {
        errors.push(format!("failed to leave alternate screen: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

pub(crate) fn restore_terminal() -> io::Result<()> {
    restore_terminal_to(&mut io::stdout(), disable_raw_mode)
}

pub(crate) type RestoreTerminalFn = fn() -> io::Result<()>;

pub(crate) struct TerminalRestoreGuard {
    pub(crate) active: bool,
    pub(crate) restore_terminal: RestoreTerminalFn,
}

impl TerminalRestoreGuard {
    pub(crate) fn active() -> Self {
        Self {
            active: true,
            restore_terminal,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_restore(restore_terminal: RestoreTerminalFn) -> Self {
        Self {
            active: true,
            restore_terminal,
        }
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        (self.restore_terminal)()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = (self.restore_terminal)();
            self.active = false;
        }
    }
}
