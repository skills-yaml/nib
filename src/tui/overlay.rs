//! TUI internals split for T043 module size.

use super::*;

pub(crate) fn render_model_selection(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    pending: &PendingModelSelection,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let safe_label = |value: &str| {
        bounded_public_text(
            value,
            &pending.selection.sensitive_values,
            MAX_SESSION_DETAIL_ITEM_CHARS,
            false,
        )
    };
    let hint_rows = u16::from(inner.height > 1);
    let capacity = usize::from(inner.height.saturating_sub(hint_rows)).max(1);
    let (start, end) = visible_option_range(
        pending.selected_option,
        pending.selection.available.len(),
        capacity,
    );
    let mut lines = if pending.selection.available.is_empty() {
        vec![list_option_line(
            "No configured suggestions; type an exact model ID.",
            false,
            inner.width,
            no_color,
        )]
    } else {
        pending.selection.available[start..end]
            .iter()
            .enumerate()
            .map(|(offset, model)| {
                let current = if model == &pending.selection.current {
                    "current"
                } else {
                    ""
                };
                two_column_option(
                    &safe_label(model),
                    current,
                    start + offset == pending.selected_option,
                    inner.width,
                    no_color,
                )
            })
            .collect()
    };
    if hint_rows > 0 {
        let typed = safe_label(&pending.response);
        let hint = if typed.is_empty() {
            "Up/Down select · type exact ID · Enter select · Esc cancel".to_string()
        } else {
            format!("ID {typed} · Enter select · Esc cancel")
        };
        lines.push(band_hint_line(&hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitcherAction {
    Pending,
    Close,
    PreviewExact(String),
    Activate,
}

pub(crate) fn session_switcher_action_for_key(
    switcher: &mut SessionSwitcher,
    code: KeyCode,
) -> SwitcherAction {
    if switcher.confirming {
        let answer = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => "y",
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => "n",
            _ => return SwitcherAction::Pending,
        };
        let state = InteractionState {
            destructive_confirmation_pending: true,
            ..InteractionState::default()
        };
        return match reduce_interaction(&state, InteractionInput::ConfirmationAnswer(answer)) {
            InteractionReduction::ConfirmationDecision(InteractionDecision::Accept) => {
                SwitcherAction::Activate
            }
            InteractionReduction::ConfirmationDecision(InteractionDecision::Reject) => {
                switcher.confirming = false;
                SwitcherAction::Pending
            }
            _ => SwitcherAction::Pending,
        };
    }
    match code {
        KeyCode::Esc => SwitcherAction::Close,
        KeyCode::Backspace => {
            switcher.exact_id.pop();
            switcher.error = None;
            SwitcherAction::Pending
        }
        KeyCode::Char(character)
            if switcher.exact_id.len().saturating_add(character.len_utf8())
                <= MAX_SWITCHER_EXACT_ID_BYTES =>
        {
            switcher.exact_id.push(character);
            switcher.error = None;
            SwitcherAction::Pending
        }
        KeyCode::Up => {
            switcher.selected = switcher.selected.saturating_sub(1);
            SwitcherAction::Pending
        }
        KeyCode::Down => {
            if !switcher.candidates.is_empty() {
                switcher.selected =
                    (switcher.selected + 1).min(switcher.candidates.len().saturating_sub(1));
            }
            SwitcherAction::Pending
        }
        KeyCode::Enter if !switcher.exact_id.trim().is_empty() => {
            SwitcherAction::PreviewExact(switcher.exact_id.trim().to_string())
        }
        KeyCode::Enter if !switcher.candidates.is_empty() => {
            switcher.confirming = true;
            SwitcherAction::Pending
        }
        _ => SwitcherAction::Pending,
    }
}

pub(crate) fn preview_exact_session(
    store: &SessionStore,
    switcher: &mut SessionSwitcher,
    active_session_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let candidate = interactive_session_candidate(store, session_id, active_session_id)?;
    if let Some(index) = switcher
        .candidates
        .iter()
        .position(|existing| existing.id == candidate.id)
    {
        switcher.candidates[index] = candidate;
        switcher.selected = index;
    } else {
        if switcher.candidates.len() >= MAX_SWITCHER_CANDIDATES {
            let replace = switcher
                .candidates
                .iter()
                .rposition(|existing| !existing.is_active)
                .ok_or_else(|| "no bounded switcher slot is available".to_string())?;
            switcher.candidates.remove(replace);
        }
        switcher.candidates.push(candidate);
        switcher.selected = switcher.candidates.len() - 1;
    }
    switcher.exact_id.clear();
    switcher.error = None;
    Ok(())
}

pub(crate) fn activate_selected_session(
    store: &SessionStore,
    switcher: &SessionSwitcher,
    worker_active: bool,
    active_session_id: &mut String,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    if worker_active {
        return Err(
            "Agent is still running; cancel it or wait before switching sessions.".to_string(),
        );
    }
    let candidate = switcher
        .candidates
        .get(switcher.selected)
        .ok_or_else(|| "No session is selected.".to_string())?;
    let target = candidate.id.clone();
    // This is deliberately a strict, preview-token-validated read, not
    // resolve_session: a stale target must never create a replacement session or
    // redirect the active workload.
    let session = validate_interactive_session_target(store, candidate).map_err(|error| {
        format!("Could not resume {target}: {error}. The active session is unchanged.")
    })?;
    let replacement =
        ActiveTimeline::from_session(&session, store.public_sensitive_values().to_vec());
    *timeline = replacement;
    *active_session_id = target.clone();
    Ok(target)
}

pub(crate) fn replace_active_session(
    store: &SessionStore,
    session_id: String,
    output: String,
    active_session_id: &mut String,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    let previous = active_session_id.clone();
    let disposition = queue_disposition_message(store, &previous, "switched sessions")?;
    let mut replacement = ActiveTimeline::load(store, &session_id)
        .map_err(|error| format!("created session could not be loaded: {error}"))?;
    replacement.push_status(output);
    replacement.push_status(disposition.clone());
    *timeline = replacement;
    *active_session_id = session_id;
    Ok(disposition)
}

pub(crate) fn render_session_switcher(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    switcher: &SessionSwitcher,
    active_session_id: &str,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let mut lines = Vec::new();
    if switcher.confirming {
        let label = switcher
            .candidates
            .get(switcher.selected)
            .map(|candidate| candidate.label.as_str())
            .unwrap_or("session");
        lines.push(two_column_option(
            &format!("Resume {label}"),
            "replaces this session",
            true,
            inner.width,
            no_color,
        ));
        if hint_rows > 0 {
            lines.push(band_hint_line(
                "Y / Enter resume · N / Esc keep current",
                inner.width,
                no_color,
            ));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if !switcher.exact_id.is_empty() && inner.height > 2 {
        lines.push(band_hint_line(
            &format!("ID {}", switcher.exact_id),
            inner.width,
            no_color,
        ));
    }
    let used = lines.len();
    let capacity = usize::from(inner.height.saturating_sub(hint_rows))
        .saturating_sub(used)
        .max(1);
    let (start, end) = visible_option_range(switcher.selected, switcher.candidates.len(), capacity);
    if switcher.candidates.is_empty() {
        lines.push(list_option_line(
            "(no sessions)",
            false,
            inner.width,
            no_color,
        ));
    } else {
        for (offset, candidate) in switcher.candidates[start..end].iter().enumerate() {
            let description = if candidate.id == active_session_id {
                "active"
            } else {
                candidate.preview.lines().next().unwrap_or("")
            };
            lines.push(two_column_option(
                &candidate.label,
                description,
                start + offset == switcher.selected,
                inner.width,
                no_color,
            ));
        }
    }
    if hint_rows > 0 {
        let error_hint = switcher
            .error
            .as_ref()
            .map(|error| format!("[switcher error] {error}"));
        let hint = if let Some(error) = error_hint.as_deref() {
            error
        } else if switcher.omitted > 0 {
            "Up/Down select · type exact ID · Enter resume · Esc close"
        } else {
            "Up/Down select · Enter resume · Esc close"
        };
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn completion_reserved_height(
    area_height: u16,
    row_count: usize,
    composer_height: u16,
    meter_height: u16,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let desired =
        u16::try_from(row_count.min(MAX_VISIBLE_COMPLETIONS).saturating_add(1)).unwrap_or(u16::MAX);
    let chrome = 1u16
        .saturating_add(3)
        .saturating_add(composer_height)
        .saturating_add(meter_height)
        .saturating_add(1);
    desired.min(area_height.saturating_sub(chrome))
}

pub(crate) fn command_approval_reserved_height(
    area_height: u16,
    row_count: usize,
    composer_height: u16,
    meter_height: u16,
) -> u16 {
    let chrome = 1u16
        .saturating_add(3)
        .saturating_add(composer_height)
        .saturating_add(meter_height)
        .saturating_add(1);
    u16::try_from(row_count)
        .unwrap_or(u16::MAX)
        .min(area_height.saturating_sub(chrome))
}

pub(crate) fn split_session_layout(
    area: Rect,
    composer_height: u16,
    meter_height: u16,
    completion_height: u16,
) -> SessionLayout {
    let mut constraints = vec![Constraint::Length(1), Constraint::Min(3)];
    if meter_height > 0 {
        constraints.push(Constraint::Length(meter_height));
    }
    constraints.push(Constraint::Length(composer_height));
    if completion_height > 0 {
        constraints.push(Constraint::Length(completion_height));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut index = 0;
    let header = chunks[index];
    index += 1;
    let transcript = chunks[index];
    index += 1;
    let meter = if meter_height > 0 {
        let rect = chunks[index];
        index += 1;
        rect
    } else {
        Rect::default()
    };
    let composer = chunks[index];
    index += 1;
    let completion = if completion_height > 0 {
        let rect = chunks[index];
        index += 1;
        rect
    } else {
        Rect::default()
    };
    SessionLayout {
        header,
        transcript,
        meter,
        composer,
        completion,
        footer: chunks[index],
    }
}

pub(crate) fn completion_inner_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let left = COMPOSER_PROMPT_CELLS.min(area.width.saturating_sub(1));
    let width = area.width.saturating_sub(left).clamp(1, 92);
    Rect {
        x: area.x.saturating_add(left),
        y: area.y,
        width,
        height: area.height,
    }
}

#[cfg(test)]
pub(crate) fn completion_rect(area: Rect, row_count: usize, composer_height: u16) -> Rect {
    let height = completion_reserved_height(area.height, row_count, composer_height, 0);
    let layout = split_session_layout(area, composer_height, 0, height);
    completion_inner_rect(layout.completion)
}

pub(crate) fn completion_signature(suggestion: &InteractiveCompletion) -> &str {
    let insertion = suggestion.insertion.trim_end();
    if suggestion.insertion.starts_with('/') && !insertion.contains(char::is_whitespace) {
        suggestion.usage
    } else {
        insertion
    }
}

pub(crate) fn selected_option_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

pub(crate) fn list_option_line(
    text: &str,
    selected: bool,
    width: u16,
    no_color: bool,
) -> Line<'static> {
    let style = if selected {
        selected_option_style(no_color)
    } else {
        Style::default()
    };
    Line::from(Span::styled(
        truncate_completion_text(text, usize::from(width.max(1))),
        style,
    ))
}

pub(crate) fn truncate_completion_text(value: &str, max_cells: usize) -> String {
    if unicode_display_width(value) <= max_cells {
        return value.to_string();
    }
    if max_cells == 0 {
        return String::new();
    }
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        let mut candidate = output.clone();
        candidate.push_str(grapheme);
        if unicode_display_width(&candidate) > max_cells.saturating_sub(1) {
            break;
        }
        output.push_str(grapheme);
    }
    output.push('…');
    output
}

pub(crate) fn two_column_text(signature: &str, description: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let signature_column = width.saturating_mul(2).checked_div(5).unwrap_or(0).max(1);
    let signature = truncate_completion_text(signature, signature_column);
    let signature_width = unicode_display_width(&signature);
    let gap = if width > signature_column { 2 } else { 0 };
    let description_width = width.saturating_sub(signature_column.saturating_add(gap));
    let description = truncate_completion_text(description, description_width);
    format!(
        "{signature}{}{description}",
        " ".repeat(
            signature_column
                .saturating_sub(signature_width)
                .saturating_add(gap)
        )
    )
}

pub(crate) fn two_column_option(
    signature: &str,
    description: &str,
    selected: bool,
    width: u16,
    no_color: bool,
) -> Line<'static> {
    list_option_line(
        &two_column_text(signature, description, width),
        selected,
        width,
        no_color,
    )
}

pub(crate) fn completion_line(suggestion: &InteractiveCompletion, width: u16) -> String {
    two_column_text(completion_signature(suggestion), suggestion.summary, width)
}

pub(crate) fn visible_option_range(selected: usize, len: usize, capacity: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let capacity = capacity.max(1);
    let selected = if selected < len { selected } else { 0 };
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    (start, start.saturating_add(capacity).min(len))
}

pub(crate) fn band_hint_line(text: &str, width: u16, no_color: bool) -> Line<'static> {
    Line::from(Span::styled(
        truncate_completion_text(text, usize::from(width.max(1))),
        muted_style(no_color),
    ))
}

pub(crate) fn numbered_choice_line(
    index: usize,
    text: &str,
    shortcut: &str,
    selected: bool,
    width: u16,
    no_color: bool,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let body = format!("{}. {text} ({shortcut})", index + 1);
    let line = truncate_completion_text(&format!("{marker}{body}"), usize::from(width.max(1)));
    let style = if selected {
        selected_option_style(no_color)
    } else if no_color {
        Style::default()
    } else {
        speech_body_style(false)
    };
    Line::from(Span::styled(line, style))
}

pub(crate) fn render_numbered_choice_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    choices: &[(String, String)],
    selected: usize,
    hint: &str,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let capacity = usize::from(inner.height.saturating_sub(hint_rows)).max(1);
    let (start, end) = visible_option_range(selected, choices.len(), capacity);
    let mut lines = choices[start..end]
        .iter()
        .enumerate()
        .map(|(offset, (text, shortcut))| {
            numbered_choice_line(
                start + offset,
                text,
                shortcut,
                start + offset == selected,
                inner.width,
                no_color,
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() || hint_rows > 0 {
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[allow(dead_code)]
pub(crate) fn render_choice_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    options: &[(&str, &str)],
    selected: usize,
    hint: &str,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let capacity = usize::from(inner.height.saturating_sub(hint_rows)).max(1);
    let (start, end) = visible_option_range(selected, options.len(), capacity);
    let mut lines = options[start..end]
        .iter()
        .enumerate()
        .map(|(offset, (signature, description))| {
            two_column_option(
                signature,
                description,
                start + offset == selected,
                inner.width,
                no_color,
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() || hint_rows > 0 {
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn command_approval_lines(req: &TuiApprovalRequest) -> Vec<String> {
    let command = req
        .context
        .shown_command
        .clone()
        .or_else(|| {
            req.call
                .arguments
                .get("command")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let environment = if req.context.command_environment.is_empty() {
        "local"
    } else {
        req.context.command_environment.as_str()
    };
    let remember = req.context.remember_exact.as_deref();
    let mut card = crate::interaction_card::command_approval_card(
        environment,
        &req.context.reason,
        &command,
        &req.context.command_extras,
        remember,
        req.selected_option,
        true,
    );
    if let Some(draft) = &req.reason_draft {
        card.text.push_str(&format!("\nReason to record: {draft}"));
    }
    if let Some(error) = req.error.as_ref().or(req.context.input_error.as_ref()) {
        card.text.push_str(&format!("\n{error}"));
    }
    card.text.lines().map(str::to_string).collect()
}

pub(crate) fn render_command_approval_card(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    req: &TuiApprovalRequest,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let lines = command_approval_lines(req);
    let capacity = usize::from(inner.height).max(1);
    let start = lines.len().saturating_sub(capacity);
    let rendered = lines[start..]
        .iter()
        .map(|line| {
            let style = if line.starts_with("› ") {
                selected_option_style(no_color)
            } else {
                Style::default()
            };
            Line::from(Span::styled(line.clone(), style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rendered), inner);
}

pub(crate) fn render_approval_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    req: &TuiApprovalRequest,
) {
    if req.call.tool_name == "run_terminal" {
        render_command_approval_card(frame, area, req);
        return;
    }
    if req.details_open {
        render_choice_band(
            frame,
            area,
            &[("Approval details", "Esc returns to the decision")],
            0,
            "Up/Down scroll · Esc back",
        );
        return;
    }
    render_numbered_choice_band(
        frame,
        area,
        &[
            ("Approve once".to_string(), "y".to_string()),
            ("Deny".to_string(), "esc".to_string()),
            ("View details".to_string(), "enter".to_string()),
        ],
        req.selected_option.min(2),
        "Up/Down select · Enter choose · Esc deny",
    );
}

pub(crate) fn render_workspace_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    _directory: &str,
    selected: usize,
) {
    render_numbered_choice_band(
        frame,
        area,
        &[
            ("Allow".to_string(), "y".to_string()),
            ("Decline and quit".to_string(), "esc".to_string()),
        ],
        selected.min(1),
        "Up/Down select · Enter choose · Esc decline",
    );
}

pub(crate) fn render_completion(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    completion: &CompletionMenu,
) {
    if !completion.is_open() || area.width == 0 || area.height == 0 {
        return;
    }
    let modal_area = completion_inner_rect(area);
    if modal_area.width == 0 || modal_area.height == 0 {
        return;
    }
    let hint_rows = u16::from(modal_area.height > 1);
    let visible_capacity = usize::from(modal_area.height.saturating_sub(hint_rows)).max(1);
    let start = completion
        .selected
        .saturating_sub(visible_capacity.saturating_sub(1));
    let end = (start + visible_capacity).min(completion.suggestions.len());
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let inner_width = modal_area.width;
    let mut lines = completion.suggestions[start..end]
        .iter()
        .enumerate()
        .map(|(offset, suggestion)| {
            let selected = start + offset == completion.selected;
            let style = if selected {
                selected_option_style(no_color)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                completion_line(suggestion, inner_width),
                style,
            ))
        })
        .collect::<Vec<_>>();
    if hint_rows > 0 {
        lines.push(Line::from(Span::styled(
            truncate_completion_text(
                "Up/Down select · Tab insert · Enter run · Esc close",
                usize::from(inner_width),
            ),
            muted_style(no_color),
        )));
    }
    frame.render_widget(Paragraph::new(lines), modal_area);
}

pub(crate) fn render_history_search(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    search: &PendingHistorySearch,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let mut lines = Vec::new();
    if inner.height > 2 {
        let query = if search.query.is_empty() {
            "type to search drafts"
        } else {
            search.query.as_str()
        };
        lines.push(band_hint_line(query, inner.width, no_color));
    }
    let used = lines.len();
    let capacity = usize::from(inner.height.saturating_sub(hint_rows))
        .saturating_sub(used)
        .max(1);
    let (start, end) = visible_option_range(search.selected, search.search.matches.len(), capacity);
    if search.search.matches.is_empty() {
        lines.push(list_option_line(
            "(no matches)",
            false,
            inner.width,
            no_color,
        ));
    } else {
        for (offset, result) in search.search.matches[start..end].iter().enumerate() {
            lines.push(list_option_line(
                &result.display,
                start + offset == search.selected,
                inner.width,
                no_color,
            ));
        }
    }
    if hint_rows > 0 {
        let hint = search
            .error
            .as_deref()
            .unwrap_or("Up/Down select · Enter restore · Esc close");
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn render_modal_state_error(frame: &mut ratatui::Frame<'_>, message: &'static str) {
    let modal_area = centered_rect(70, 30, frame.area());
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    frame.render_widget(
        Paragraph::new(message)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Recoverable UI Error "),
            )
            .wrap(ratatui::widgets::Wrap { trim: true }),
        modal_area,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_interaction_overlay(
    frame: &mut ratatui::Frame<'_>,
    layer: InteractionLayer,
    pending_model: Option<&PendingModelSelection>,
    pending_switcher: Option<&SessionSwitcher>,
    pending_history_search: Option<&PendingHistorySearch>,
    _active_session_id: &str,
) {
    match layer {
        InteractionLayer::Model if pending_model.is_none() => render_modal_state_error(
            frame,
            "Model selector state is unavailable; press Esc to continue.",
        ),
        InteractionLayer::SessionConfirmation | InteractionLayer::SessionSwitcher
            if pending_switcher.is_none() =>
        {
            render_modal_state_error(
                frame,
                "Session selector state is unavailable; press Esc to continue.",
            );
        }
        InteractionLayer::HistorySearch if pending_history_search.is_none() => {
            render_modal_state_error(
                frame,
                "Draft history state is unavailable; press Esc to continue.",
            );
        }
        InteractionLayer::RecoverableError => render_modal_state_error(
            frame,
            "Interaction state is unavailable; press Esc to continue.",
        ),
        InteractionLayer::Model
        | InteractionLayer::SessionConfirmation
        | InteractionLayer::SessionSwitcher
        | InteractionLayer::HistorySearch
        | InteractionLayer::Approval
        | InteractionLayer::Question
        | InteractionLayer::Composer
        | InteractionLayer::Completion => {}
    }
}
