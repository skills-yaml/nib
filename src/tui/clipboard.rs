//! TUI internals split for T043 module size.

use super::*;

pub(crate) fn copy_text_osc52(text: &str) -> bool {
    let mut out = io::stdout();
    write!(out, "{}", osc52_sequence(text)).is_ok() && out.flush().is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardDelivery {
    Native,
    Osc52Unconfirmed,
    Unavailable,
    Failed,
}

impl ClipboardDelivery {
    pub(crate) fn status(self) -> &'static str {
        match self {
            Self::Native => "Copied",
            Self::Osc52Unconfirmed => "Copy requested via OSC52 (unconfirmed)",
            Self::Unavailable => {
                "Copy unavailable: output is not a terminal; text remains available for manual selection"
            }
            Self::Failed => "Copy failed; text remains available for manual selection",
        }
    }

    pub(crate) fn may_clear_selection(self) -> bool {
        matches!(self, Self::Native | Self::Osc52Unconfirmed)
    }
}

pub(crate) fn deliver_text_to_clipboard(text: &str) -> ClipboardDelivery {
    deliver_text_to_clipboard_with(
        text,
        io::stdout().is_terminal(),
        copy_text_system_clipboard,
        copy_text_osc52,
    )
}

pub(crate) fn deliver_text_to_clipboard_with<Native, Osc52>(
    text: &str,
    is_terminal: bool,
    mut native: Native,
    mut osc52: Osc52,
) -> ClipboardDelivery
where
    Native: FnMut(&str) -> bool,
    Osc52: FnMut(&str) -> bool,
{
    if !is_terminal {
        ClipboardDelivery::Unavailable
    } else if native(text) {
        ClipboardDelivery::Native
    } else if osc52(text) {
        ClipboardDelivery::Osc52Unconfirmed
    } else {
        ClipboardDelivery::Failed
    }
}

pub fn copy_text_to_clipboard(text: &str) -> &'static str {
    deliver_text_to_clipboard(text).status()
}

pub(crate) fn copy_text_system_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return pipe_stdin_to_command(text, "pbcopy", &[]);
    }
    #[cfg(windows)]
    {
        return pipe_stdin_to_command(text, "clip", &[]);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        pipe_stdin_to_command(text, "wl-copy", &[])
            || pipe_stdin_to_command(text, "xclip", &["-selection", "clipboard"])
            || pipe_stdin_to_command(text, "xsel", &["--clipboard", "--input"])
    }
}

pub(crate) fn pipe_stdin_to_command(text: &str, program: &str, args: &[&str]) -> bool {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    matches!(child.wait(), Ok(status) if status.success()) && wrote
}

pub(crate) fn publish_copied_chat(
    pointer: Option<PointerSelection>,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
) -> Option<&'static str> {
    let (text, _status) = copy_chat_content(pointer, view, selected, activities)?;
    Some(deliver_text_to_clipboard(&text).status())
}

pub(crate) fn copy_pointer_selection_and_clear(
    pointer_selection: &mut Option<PointerSelection>,
    tui_focus: &mut TuiFocus,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
) -> Option<&'static str> {
    copy_pointer_selection_and_clear_with(
        pointer_selection,
        tui_focus,
        view,
        selected,
        activities,
        deliver_text_to_clipboard,
    )
}

pub(crate) fn copy_pointer_selection_and_clear_with<Deliver>(
    pointer_selection: &mut Option<PointerSelection>,
    tui_focus: &mut TuiFocus,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
    deliver: Deliver,
) -> Option<&'static str>
where
    Deliver: FnOnce(&str) -> ClipboardDelivery,
{
    let (text, _status) = copy_chat_content(*pointer_selection, view, selected, activities)?;
    let delivery = deliver(&text);
    if delivery.may_clear_selection() {
        *pointer_selection = None;
        *tui_focus = TuiFocus::Composer;
    }
    Some(delivery.status())
}

pub(crate) fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", encode_base64(text.as_bytes()))
}

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let remaining = bytes.len() - index;
        let b0 = bytes[index];
        let b1 = if remaining > 1 { bytes[index + 1] } else { 0 };
        let b2 = if remaining > 2 { bytes[index + 2] } else { 0 };
        output.push(TABLE[(b0 >> 2) as usize] as char);
        output.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if remaining == 1 {
            output.push('=');
            output.push('=');
        } else {
            output.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            if remaining == 2 {
                output.push('=');
            } else {
                output.push(TABLE[(b2 & 0x3f) as usize] as char);
            }
        }
        index += 3;
    }
    output
}
