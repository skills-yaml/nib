//! TUI internals split for T043 module size.

use super::*;

pub(crate) fn transcript_action_for_key(
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<TranscriptViewportAction> {
    match code {
        KeyCode::PageUp => Some(TranscriptViewportAction::PageUp),
        KeyCode::PageDown => Some(TranscriptViewportAction::PageDown),
        KeyCode::Home if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(TranscriptViewportAction::JumpToStart)
        }
        KeyCode::End if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(TranscriptViewportAction::JumpToEnd)
        }
        KeyCode::Up
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(TranscriptViewportAction::Lines(-1))
        }
        KeyCode::Down
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(TranscriptViewportAction::Lines(1))
        }
        _ => None,
    }
}

pub(crate) fn transcript_action_for_mouse(
    kind: MouseEventKind,
) -> Option<TranscriptViewportAction> {
    match kind {
        MouseEventKind::ScrollUp => Some(TranscriptViewportAction::Lines(-3)),
        MouseEventKind::ScrollDown => Some(TranscriptViewportAction::Lines(3)),
        _ => None,
    }
}

pub(crate) fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && y >= area.y
        && x < area.x.saturating_add(area.width)
        && y < area.y.saturating_add(area.height)
}

pub(crate) fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

pub(crate) fn slice_display_cells(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for grapheme in text.graphemes(true) {
        let width = unicode_display_width(grapheme);
        if col >= end {
            break;
        }
        if col >= start {
            out.push_str(grapheme);
        }
        col = col.saturating_add(width);
    }
    out
}

pub(crate) fn extract_pointer_text(rows: &[String], selection: PointerSelection) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let ((start_row, start_col), (end_row, end_col)) = selection.ordered();
    let start_row = start_row.min(rows.len().saturating_sub(1));
    let end_row = end_row.min(rows.len().saturating_sub(1));
    if start_row == end_row {
        let end = end_col.max(start_col.saturating_add(1));
        return slice_display_cells(&rows[start_row], start_col, end);
    }
    let mut parts = Vec::new();
    parts.push(slice_display_cells(&rows[start_row], start_col, usize::MAX));
    parts.extend(
        rows.iter()
            .take(end_row)
            .skip(start_row.saturating_add(1))
            .cloned(),
    );
    parts.push(slice_display_cells(&rows[end_row], 0, end_col.max(1)));
    parts.join("\n")
}

pub(crate) fn last_assistant_copy(activities: &[ActivityEntry]) -> Option<String> {
    activities.iter().rev().find_map(|entry| {
        if entry.kind == ActivityKind::Assistant {
            let text = entry.copy_text();
            (!text.trim().is_empty()).then_some(text)
        } else {
            None
        }
    })
}

pub(crate) fn copy_chat_content(
    pointer: Option<PointerSelection>,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
) -> Option<(String, &'static str)> {
    if let Some(selection) = pointer.filter(|selection| selection.moved) {
        let text = extract_pointer_text(&view.plain_rows, selection);
        if !text.trim().is_empty() {
            return Some((text, "Copied selection."));
        }
    }
    if let Some(index) = selected {
        if let Some(entry) = activities.get(index) {
            let text = entry.copy_text();
            if !text.is_empty() {
                return Some((text, "Copied."));
            }
        }
    }
    if let Some(text) = last_assistant_copy(activities) {
        return Some((text, "Copied last reply."));
    }
    let joined = view
        .plain_rows
        .iter()
        .filter(|row| !row.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some((joined, "Copied chat."))
    }
}

pub(crate) fn transcript_hit(view: &TranscriptView, mouse: MouseEvent) -> Option<(usize, usize)> {
    if view.area.width == 0 || view.area.height == 0 || view.plain_rows.is_empty() {
        return None;
    }
    if !point_in_rect(view.area, mouse.column, mouse.row) {
        return None;
    }
    let row = view
        .top_row
        .saturating_add(usize::from(mouse.row.saturating_sub(view.area.y)));
    if row >= view.plain_rows.len() {
        return None;
    }
    let col = usize::from(mouse.column.saturating_sub(view.area.x))
        .min(unicode_display_width(&view.plain_rows[row]));
    Some((row, col))
}
