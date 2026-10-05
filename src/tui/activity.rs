//! TUI internals split for T043 module size.

use super::*;

#[cfg(test)]
pub(crate) fn parse_role_line(line: &str) -> Option<(ActivityKind, &str)> {
    for kind in [
        ActivityKind::User,
        ActivityKind::Assistant,
        ActivityKind::Thinking,
        ActivityKind::Plan,
        ActivityKind::Tool,
        ActivityKind::Approval,
        ActivityKind::Question,
        ActivityKind::Compression,
        ActivityKind::Reconcile,
        ActivityKind::Cancellation,
        ActivityKind::Failure,
        ActivityKind::System,
    ] {
        let label = if matches!(kind, ActivityKind::Thinking | ActivityKind::Plan) {
            "thought"
        } else {
            kind.role_label()
        };
        if line == label || line == kind.role_label() {
            return Some((kind, ""));
        }
        for prefix in [format!("{label}  "), format!("{}  ", kind.role_label())] {
            if let Some(rest) = line.strip_prefix(&prefix) {
                return Some((kind, rest));
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn activities_from_timeline_text(text: &str) -> Vec<ActivityEntry> {
    let mut entries = Vec::new();
    let mut current: Option<ActivityEntry> = None;
    for line in text.lines() {
        if let Some((kind, rest)) = parse_role_line(line) {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(ActivityEntry::new(kind, rest.to_string(), String::new()));
        } else if line.is_empty() {
            continue;
        } else if let Some(entry) = current.as_mut() {
            if !entry.body.is_empty() {
                entry.body.push('\n');
            }
            entry.body.push_str(line);
        } else {
            entries.push(ActivityEntry::new(
                ActivityKind::System,
                line.to_string(),
                String::new(),
            ));
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

pub(crate) fn apply_selection(
    mut line: Line<'static>,
    selected: bool,
    no_color: bool,
) -> Line<'static> {
    if selected {
        if no_color {
            line.spans
                .iter_mut()
                .for_each(|span| span.style = span.style.add_modifier(Modifier::REVERSED));
        } else {
            line.spans.iter_mut().for_each(|span| {
                span.style = span.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            });
        }
    }
    line
}

pub(crate) fn speech_source(entry: &ActivityEntry) -> String {
    if entry.title.is_empty() || entry.title == "live" {
        entry.body.clone()
    } else if entry.body.is_empty() || entry.folded {
        entry.title.clone()
    } else {
        format!("{}\n{}", entry.title, entry.body)
    }
}

pub(crate) fn prefix_speech_dot(
    mut lines: Vec<Line<'static>>,
    kind: ActivityKind,
    no_color: bool,
) -> Vec<Line<'static>> {
    let dot = Span::styled(
        CHANNEL_DOT.to_string(),
        ink_style(channel_ink(kind), no_color),
    );
    if lines.is_empty() {
        return vec![Line::from(dot)];
    }
    let indent = markdown::speech_indent();
    let first = &mut lines[0];
    if let Some(span) = first.spans.first_mut() {
        if span.content.as_ref() == indent {
            first.spans.remove(0);
        } else if let Some(rest) = span.content.strip_prefix(indent) {
            span.content = rest.to_string().into();
        }
    }
    let rest = std::mem::take(&mut first.spans);
    first.spans.push(dot);
    first.spans.extend(rest);
    lines
}

pub(crate) fn speech_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
) -> Vec<Line<'static>> {
    let source = speech_source(entry);
    if source.is_empty() {
        return dotted_header_lines(entry.kind, "", "", width, no_color);
    }
    let mut lines = prefix_speech_dot(
        markdown::render_markdown(&source, width, markdown::speech_indent(), no_color),
        entry.kind,
        no_color,
    );
    if no_color {
        let label = entry.kind.role_label();
        if let Some(first) = lines.first_mut() {
            let mut spans = vec![Span::raw(format!("{label}  "))];
            spans.append(&mut first.spans);
            *first = Line::from(spans);
        }
    }
    lines
}

pub(crate) fn plan_todo_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
) -> Vec<Line<'static>> {
    let mut lines = dotted_header_lines(entry.kind, &entry.title, "", width, no_color);
    let indent = "  ";
    let inner = width.saturating_sub(2).max(1);
    for line in entry.body.lines() {
        let style = if line.starts_with('◐') || line.starts_with('!') || line.starts_with('×') {
            ink_style(ChannelInk::ToolCall, no_color)
        } else if line.starts_with('✓') {
            muted_style(no_color)
        } else {
            thought_body_style(no_color)
        };
        for wrapped in wrapped_display_rows(line, inner) {
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(wrapped, style),
            ]));
        }
    }
    lines
}

pub(crate) fn thought_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> Vec<Line<'static>> {
    let marker = if entry.folded { "▸ " } else { "▾ " };
    let heading = thought_row_title(entry, live);
    let style = thought_body_style(no_color);
    let inner = usize::from(width.max(1))
        .saturating_sub(unicode_display_width(marker))
        .max(1);
    let mut lines: Vec<Line<'static>> =
        wrapped_display_rows(&heading, u16::try_from(inner).unwrap_or(u16::MAX))
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                if index == 0 {
                    Line::from(vec![
                        Span::styled(marker.to_string(), muted_style(no_color)),
                        Span::styled(row, style),
                    ])
                } else {
                    Line::from(Span::styled(
                        format!("{}{row}", " ".repeat(unicode_display_width(marker))),
                        style,
                    ))
                }
            })
            .collect();
    if !entry.folded && !entry.body.is_empty() {
        lines.extend(dotted_result_lines(
            &entry.body,
            width,
            ChannelInk::Thought,
            no_color,
        ));
    }
    lines
}

pub(crate) fn running_tool_verb(title: &str) -> &'static str {
    if title.starts_with("run_terminal") {
        "Running command…"
    } else if title.starts_with("read_file")
        || title.starts_with("list_directory")
        || title.starts_with("grep")
    {
        "Reading…"
    } else {
        "Working…"
    }
}

pub(crate) fn tool_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> Vec<Line<'static>> {
    let rest = quiet_tool_title(&entry.title);
    let mut lines = dotted_header_lines(entry.kind, &rest, fold_prefix(entry), width, no_color);
    if tool_is_live(entry) {
        let spin = spinner_glyph(live.map(|value| value.tick).unwrap_or(0), no_color);
        let verb = running_tool_verb(&entry.title);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{spin}  {verb}"),
                ink_style(ChannelInk::ToolCall, no_color),
            ),
        ]));
    }
    if !entry.folded && !entry.body.is_empty() {
        lines.extend(dotted_result_lines(
            &entry.body,
            width,
            ChannelInk::ToolResult,
            no_color,
        ));
    }
    lines
}

pub(crate) fn log_lines(entry: &ActivityEntry, width: u16, no_color: bool) -> Vec<Line<'static>> {
    let rest = if entry.title.is_empty() {
        entry.body.as_str()
    } else {
        entry.title.as_str()
    };
    let mut lines = dotted_header_lines(entry.kind, rest, fold_prefix(entry), width, no_color);
    if !entry.folded && !entry.title.is_empty() && !entry.body.is_empty() {
        lines.extend(dotted_result_lines(
            &entry.body,
            width,
            channel_ink(entry.kind),
            no_color,
        ));
    }
    lines
}

pub(crate) fn activity_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> Vec<Line<'static>> {
    match entry.kind {
        ActivityKind::User | ActivityKind::Assistant => speech_lines(entry, width, no_color),
        ActivityKind::Thinking => thought_lines(entry, width, no_color, live),
        ActivityKind::Plan => plan_todo_lines(entry, width, no_color),
        ActivityKind::Tool => tool_lines(entry, width, no_color, live),
        _ => log_lines(entry, width, no_color),
    }
}

pub(crate) fn flatten_activity_lines(
    activities: &[ActivityEntry],
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut owners = Vec::new();
    let mut previous_channel = None;
    for (index, entry) in activities.iter().enumerate() {
        let wrapped = activity_lines(entry, width, no_color, live);
        if wrapped.is_empty() {
            continue;
        }
        let channel = visual_channel(entry.kind);
        if previous_channel.is_some_and(|previous| previous != channel) {
            rows.push(Line::from(""));
            owners.push(index);
        }
        previous_channel = Some(channel);
        for row in wrapped {
            rows.push(row);
            owners.push(index);
        }
    }
    (rows, owners)
}

pub(crate) fn flatten_activity_lines_reusing(
    activities: &[ActivityEntry],
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
    previous: Option<&TranscriptRenderCache>,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<CachedActivityRows>) {
    let mut rows = Vec::new();
    let mut owners = Vec::new();
    let mut activity_rows = Vec::with_capacity(activities.len());
    let mut previous_channel = None;
    for (index, entry) in activities.iter().enumerate() {
        let old = previous.and_then(|cache| cache.activity_rows.get(index));
        let live_sensitive = entry.live_since.is_some()
            || old.is_some_and(|cached| cached.source.live_since.is_some())
            || tool_is_live(entry)
            || entry.kind == ActivityKind::Thinking
                && (entry.title.is_empty() || is_legacy_thought_title(&entry.title));
        let unchanged = old.filter(|cached| cached.source.as_ref() == entry);
        let lines = unchanged
            .filter(|_| !live_sensitive)
            .map(|cached| Arc::clone(&cached.lines))
            .unwrap_or_else(|| Arc::new(activity_lines(entry, width, no_color, live)));
        if !lines.is_empty() {
            let channel = visual_channel(entry.kind);
            if previous_channel.is_some_and(|previous| previous != channel) {
                rows.push(Line::from(""));
                owners.push(index);
            }
            previous_channel = Some(channel);
            for line in lines.iter() {
                rows.push(line.clone());
                owners.push(index);
            }
        }
        activity_rows.push(CachedActivityRows {
            source: unchanged
                .map(|cached| Arc::clone(&cached.source))
                .unwrap_or_else(|| Arc::new(entry.clone())),
            lines,
        });
    }
    (rows, owners, activity_rows)
}

pub(crate) fn ensure_selected_visible(
    viewport: &mut TranscriptViewport,
    owners: &[usize],
    selected: usize,
) {
    let Some(first) = owners.iter().position(|owner| *owner == selected) else {
        return;
    };
    let last = owners
        .iter()
        .rposition(|owner| *owner == selected)
        .unwrap_or(first);
    let page = viewport.page_rows().max(1);
    let top = viewport.top_row();
    let bottom = top.saturating_add(page.saturating_sub(1));
    if first < top {
        viewport.reveal_row(first);
    } else if last > bottom {
        viewport.reveal_row(last.saturating_sub(page.saturating_sub(1)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceConsentAction {
    Unhandled,
    SelectionChanged,
    Allow,
    Decline,
}

pub(crate) fn workspace_consent_action_for_key(
    selected: &mut usize,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> WorkspaceConsentAction {
    match code {
        KeyCode::Up => {
            *selected = 0;
            return WorkspaceConsentAction::SelectionChanged;
        }
        KeyCode::Down | KeyCode::Tab => {
            *selected = 1;
            return WorkspaceConsentAction::SelectionChanged;
        }
        _ => {}
    }
    let control_quit = matches!(code, KeyCode::Char('c' | 'C' | 'q' | 'Q'))
        && modifiers.contains(KeyModifiers::CONTROL);
    let allow = (matches!(code, KeyCode::Char('y' | 'Y' | '1'))
        || (code == KeyCode::Enter && *selected == 0))
        && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT);
    let decline = control_quit
        || matches!(code, KeyCode::Esc | KeyCode::Char('n' | 'N' | '2'))
        || (code == KeyCode::Enter && *selected != 0);
    if allow {
        WorkspaceConsentAction::Allow
    } else if decline {
        WorkspaceConsentAction::Decline
    } else {
        WorkspaceConsentAction::Unhandled
    }
}

pub(crate) fn pending_workspace_consent(project_root: &Path) -> Option<String> {
    (!crate::config::workspace_access_is_granted(project_root).unwrap_or(false))
        .then(|| crate::interactive::folder_label(project_root))
}

pub(crate) fn grant_workspace_access_and_release_goal(
    project_root: &Path,
    consent_directory: &mut Option<String>,
    pending_goal: &mut Option<String>,
) -> Result<Option<String>, crate::config::ConfigError> {
    crate::config::grant_workspace_access(project_root)?;
    *consent_directory = None;
    Ok(pending_goal.take())
}

#[cfg(test)]
pub(crate) fn render_current_session_view(
    frame: &mut ratatui::Frame<'_>,
    _header: &str,
    _status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
) {
    let mut viewport = TranscriptViewport::default();
    render_current_session_view_with_viewport(
        frame,
        _header,
        _status,
        timeline_text,
        composer,
        pending_approval,
        pending_question,
        false,
        &mut viewport,
    );
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_current_session_view_with_viewport(
    frame: &mut ratatui::Frame<'_>,
    _header: &str,
    _status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    run_active: bool,
    viewport: &mut TranscriptViewport,
) {
    let activities = activities_from_timeline_text(timeline_text);
    let welcome = StartupWelcome::fixture();
    render_session_activities(
        frame,
        &TuiChrome::fixture(),
        &activities,
        composer,
        pending_approval,
        pending_question,
        run_active,
        viewport,
        TuiFocus::Composer,
        None,
        0,
        None,
        None,
        &welcome,
        None,
        None,
        None,
    );
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_session_view_with_completion(
    frame: &mut ratatui::Frame<'_>,
    _header: &str,
    _status: &str,
    timeline_text: &str,
    composer: &Composer,
    completion: Option<&CompletionMenu>,
    meter: Option<&WaitingMeter>,
    run_active: bool,
) {
    let mut viewport = TranscriptViewport::default();
    let activities = activities_from_timeline_text(timeline_text);
    let welcome = StartupWelcome::fixture();
    render_session_activities(
        frame,
        &TuiChrome::fixture(),
        &activities,
        composer,
        None,
        None,
        run_active,
        &mut viewport,
        TuiFocus::Composer,
        None,
        0,
        completion,
        meter,
        &welcome,
        None,
        None,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn render_session_activities(
    frame: &mut ratatui::Frame<'_>,
    chrome: &TuiChrome,
    activities: &[ActivityEntry],
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    _run_active: bool,
    viewport: &mut TranscriptViewport,
    focus: TuiFocus,
    selected: Option<usize>,
    queued: usize,
    completion: Option<&CompletionMenu>,
    meter: Option<&WaitingMeter>,
    welcome: &StartupWelcome,
    band: Option<&InteractionBand<'_>>,
    pointer: Option<PointerSelection>,
    mut view: Option<&mut TranscriptView>,
) {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let waiting = if pending_approval.is_some() {
        WaitingKind::Approval
    } else if pending_question.is_some() {
        WaitingKind::Question
    } else if welcome.consent_directory.is_some() {
        WaitingKind::Workspace
    } else {
        WaitingKind::None
    };
    let overlay = waiting_composer_rows(
        waiting,
        pending_approval,
        pending_question,
        welcome.consent_directory.as_deref(),
        frame.area().width,
    );
    let composer_h = overlay
        .as_ref()
        .map(|(rows, _)| u16::try_from(rows.len().clamp(2, 6)).unwrap_or(6))
        .unwrap_or_else(|| composer_height(composer, frame.area().width))
        .saturating_add(COMPOSER_BORDER_ROWS);
    let meter_h = u16::from(meter.is_some());
    let completion = completion.filter(|menu| menu.is_open());
    let completion_band = completion.map(InteractionBand::Completion);
    let waiting_band = pending_approval
        .map(InteractionBand::Approval)
        .or_else(|| pending_question.map(InteractionBand::Question))
        .or_else(|| {
            welcome
                .consent_directory
                .as_deref()
                .map(|directory| InteractionBand::Workspace {
                    directory,
                    selected: welcome.consent_selected,
                })
        });
    let band = waiting_band.as_ref().or(band).or(completion_band.as_ref());
    let completion_h = band
        .map(|band| {
            if matches!(band, InteractionBand::Question(_))
                || matches!(band, InteractionBand::Approval(req) if req.call.tool_name == "run_terminal") {
                command_approval_reserved_height(
                    frame.area().height,
                    band.row_count(),
                    composer_h,
                    meter_h,
                )
            } else {
                completion_reserved_height(
                    frame.area().height,
                    band.row_count(),
                    composer_h,
                    meter_h,
                )
            }
        })
        .unwrap_or(0);
    let layout = split_session_layout(frame.area(), composer_h, meter_h, completion_h);
    let mut chrome = chrome.clone();
    if waiting != WaitingKind::None {
        chrome.agent_mode = agent_mode_label(waiting, None).to_string();
    }
    frame.render_widget(
        Paragraph::new(header_line(&chrome, layout.header.width, no_color)),
        layout.header,
    );
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1)])
        .split(layout.transcript);
    let empty = activities.is_empty();
    let live = meter.map(|meter| TranscriptLive {
        elapsed: meter.elapsed,
        tokens: meter_token_label(&meter.tokens),
        tick: meter.tick / 1_000,
    });
    let width = body[0].width.max(1);
    let (rendered_rows, owners) = if let Some(view) = view.as_deref_mut() {
        let rebuild = !view
            .render_cache
            .as_ref()
            .is_some_and(|cache| cache.matches(view, width, no_color, live.as_ref()));
        if rebuild {
            let previous = view.render_cache.as_ref().filter(|cache| {
                cache.session_id == view.source_session
                    && cache.width == width
                    && cache.no_color == no_color
            });
            let (rows, owners, activity_rows) = if empty {
                (
                    vec![Line::from(""); empty_state_lines(welcome, no_color).len()],
                    Vec::new(),
                    Vec::new(),
                )
            } else {
                flatten_activity_lines_reusing(activities, width, no_color, live.as_ref(), previous)
            };
            view.plain_rows = rows.iter().map(line_plain_text).collect();
            view.owners = owners.clone();
            view.render_cache = Some(TranscriptRenderCache {
                session_id: view.source_session.clone(),
                generation: view.source_generation,
                width,
                no_color,
                live_second: live.as_ref().map(|state| state.elapsed.as_secs()),
                live_tokens: live.as_ref().and_then(|state| state.tokens.clone()),
                rows: Arc::new(rows),
                owners: Arc::new(owners),
                activity_rows,
            });
        }
        let cache = view
            .render_cache
            .as_ref()
            .expect("render cache initialized");
        (Arc::clone(&cache.rows), Arc::clone(&cache.owners))
    } else {
        let (rows, owners) = if empty {
            (
                vec![Line::from(""); empty_state_lines(welcome, no_color).len()],
                Vec::new(),
            )
        } else {
            flatten_activity_lines(activities, width, no_color, live.as_ref())
        };
        (Arc::new(rows), Arc::new(owners))
    };
    viewport.observe_layout(rendered_rows.len(), usize::from(body[0].height.max(1)));
    if let Some(selected) = selected {
        ensure_selected_visible(viewport, &owners, selected);
    }
    if let Some(view) = view {
        view.area = body[0];
        view.composer_area = layout.composer;
        view.top_row = viewport.top_row();
    }
    if empty {
        let welcome_lines = empty_state_lines(welcome, no_color);
        let welcome_height = u16::try_from(welcome_lines.len()).unwrap_or(1);
        let welcome_area = Rect {
            x: body[0].x.saturating_add(1),
            y: body[0].y,
            width: body[0].width.saturating_sub(1),
            height: welcome_height.min(body[0].height),
        };
        frame.render_widget(
            Paragraph::new(welcome_lines).alignment(Alignment::Left),
            welcome_area,
        );
    } else {
        let visible_start = viewport.top_row();
        let visible_end = visible_start
            .saturating_add(usize::from(body[0].height.max(1)))
            .min(rendered_rows.len());
        let lines = rendered_rows[visible_start..visible_end]
            .iter()
            .enumerate()
            .map(|(offset, row)| {
                let row_index = visible_start + offset;
                let activity_index = owners.get(row_index).copied();
                let pointer_selected =
                    pointer.is_some_and(|selection| selection.covers_row(row_index));
                let activity_selected = selected.is_some()
                    && activity_index == selected
                    && focus == TuiFocus::Transcript
                    && !pointer.is_some_and(|selection| selection.moved);
                apply_selection(row.clone(), pointer_selected || activity_selected, no_color)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), body[0]);
    }
    if let Some(meter) = meter {
        render_waiting_meter(frame, layout.meter, meter, no_color);
    }
    let composer_area = layout.composer;
    if composer_area.width > 0 && composer_area.height > 0 {
        let border_style = composer_border_style(focus, no_color);
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style);
        let inner = block.inner(composer_area);
        frame.render_widget(block, composer_area);
        if inner.width > 0 && inner.height > 0 {
            let (rows, show_cursor) = overlay
                .as_ref()
                .map(|(rows, editable)| (rows.clone(), *editable))
                .unwrap_or_else(|| (composer_visual_rows(composer, inner.width), true));
            let (cx, cursor_row) = if overlay.is_some() {
                if let Some(question) = pending_question.filter(|question| {
                    waiting == WaitingKind::Question && question.state.editor.is_some()
                }) {
                    (u16::try_from(rows.last().map_or(0, |row| unicode_display_width(row))).unwrap_or(u16::MAX), u16::try_from(rows.len().saturating_sub(1)).unwrap_or(u16::MAX))
                } else {
                    (COMPOSER_PROMPT_CELLS, 0)
                }
            } else {
                composer_cursor_cell(&composer.input, composer.cursor, inner.width)
            };
            let visible_height = usize::from(inner.height);
            let first_visible = usize::from(cursor_row)
                .saturating_sub(visible_height.saturating_sub(1))
                .min(rows.len().saturating_sub(visible_height));
            let placeholder = overlay.is_none() && composer.input.is_empty();
            let muted_first = overlay.as_ref().is_some_and(|(_, editable)| {
                *editable && pending_question.is_some_and(|question| question.state.editor.as_ref().is_some_and(|editor| editor.text.is_empty()))
            });
            let lines = rows
                .iter()
                .skip(first_visible)
                .take(visible_height)
                .enumerate()
                .map(|(offset, row)| {
                    if first_visible == 0 && offset == 0 {
                        let input = row.strip_prefix("> ").unwrap_or(row);
                        let mut spans =
                            vec![Span::styled("> ", role_style(ActivityKind::User, no_color))];
                        if placeholder {
                            spans.push(Span::styled("Ask nib anything…", muted_style(no_color)));
                        } else if muted_first {
                            spans.push(Span::styled(input.to_string(), muted_style(no_color)));
                        } else if waiting == WaitingKind::Approval
                            || waiting == WaitingKind::Workspace
                        {
                            spans.push(Span::styled(
                                input.to_string(),
                                approval_dock_style(no_color),
                            ));
                        } else {
                            spans.push(Span::styled(
                                input.to_string(),
                                Style::default().add_modifier(Modifier::BOLD),
                            ));
                        }
                        Line::from(spans)
                    } else {
                        Line::from(Span::styled(
                            row.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ))
                    }
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), inner);
            if focus == TuiFocus::Composer && show_cursor {
                let cy = usize::from(cursor_row).saturating_sub(first_visible);
                frame.set_cursor_position(Position {
                    x: inner
                        .x
                        .saturating_add(cx.min(inner.width.saturating_sub(1))),
                    y: inner.y.saturating_add(
                        u16::try_from(cy)
                            .unwrap_or(u16::MAX)
                            .min(inner.height.saturating_sub(1)),
                    ),
                });
            }
        }
    }
    match band {
        Some(InteractionBand::Completion(menu)) => {
            render_completion(frame, layout.completion, menu);
        }
        Some(InteractionBand::Model(model)) => {
            render_model_selection(frame, layout.completion, model);
        }
        Some(InteractionBand::Sessions { switcher, active }) => {
            render_session_switcher(frame, layout.completion, switcher, active);
        }
        Some(InteractionBand::History(search)) => {
            render_history_search(frame, layout.completion, search);
        }
        Some(InteractionBand::Approval(req)) => {
            render_approval_band(frame, layout.completion, req);
        }
        Some(InteractionBand::Question(question)) => {
            render_question_band(frame, layout.completion, question);
        }
        Some(InteractionBand::Workspace {
            directory,
            selected,
        }) => {
            render_workspace_band(frame, layout.completion, directory, *selected);
        }
        None => {}
    }
    let command_approval = matches!(
        band,
        Some(InteractionBand::Approval(request)) if request.call.tool_name == "run_terminal"
    );
    let footer = if command_approval {
        footer_line(
            &chrome,
            viewport,
            queued,
            WaitingKind::None,
            Some("Press enter to confirm or esc to cancel"),
            pointer_selection_is_active(pointer),
        )
    } else {
        footer_line(
            &chrome,
            viewport,
            queued,
            waiting,
            band.map(InteractionBand::footer_hint),
            pointer_selection_is_active(pointer),
        )
    };
    frame.render_widget(
        Paragraph::new(footer).style(muted_style(no_color)),
        layout.footer,
    );
}

pub(crate) fn composer_border_style(focus: TuiFocus, no_color: bool) -> Style {
    if focus == TuiFocus::Composer {
        if no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            role_style(ActivityKind::User, false)
        }
    } else {
        muted_style(no_color)
    }
}
