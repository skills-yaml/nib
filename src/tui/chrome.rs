//! TUI internals split for T043 module size.

use super::*;
use crate::config::keymap::KeyAction;

pub(crate) fn composer_height(composer: &Composer, width: u16) -> u16 {
    let visual = composer_visual_rows(composer, width).len().clamp(2, 6);
    u16::try_from(visual).unwrap_or(6)
}

pub(crate) fn composer_cursor_cell(input: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = usize::from(width.max(1));
    let cursor = cursor.min(input.len());
    let prefix = format!("> {}", &input[..cursor]);
    let rows = wrapped_display_rows(&prefix, width as u16);
    let last_width = rows.last().map_or(0, |row| unicode_display_width(row));
    if last_width >= width {
        return (0, u16::try_from(rows.len()).unwrap_or(u16::MAX));
    }
    (
        u16::try_from(last_width).unwrap_or(width as u16 - 1),
        u16::try_from(rows.len().saturating_sub(1)).unwrap_or(u16::MAX),
    )
}

pub(crate) fn composer_visual_rows(composer: &Composer, width: u16) -> Vec<String> {
    let mut rows = wrapped_display_rows(&format!("> {}", composer.input), width.max(1));
    let (_, cursor_row) = composer_cursor_cell(&composer.input, composer.cursor, width);
    let required = usize::from(cursor_row).saturating_add(1);
    if rows.len() < required {
        rows.resize(required, String::new());
    }
    rows
}

pub(crate) fn overlay_visual_rows(first: &str, extra: &[String], width: u16) -> Vec<String> {
    const MAX_ROWS: usize = 6;
    let width = width.max(1);
    let mut rows = wrapped_display_rows(&format!("> {first}"), width);
    let inner = width.saturating_sub(COMPOSER_PROMPT_CELLS).max(1);
    let extras: Vec<&str> = extra
        .iter()
        .map(String::as_str)
        .filter(|line| !line.is_empty())
        .collect();
    let mut used = 0usize;
    while used < extras.len() {
        let remaining = extras.len() - used;
        let item_rows: Vec<String> = wrapped_display_rows(extras[used], inner)
            .into_iter()
            .map(|wrapped| format!("  {wrapped}"))
            .collect();
        let limit = if remaining > 1 {
            MAX_ROWS.saturating_sub(1)
        } else {
            MAX_ROWS
        };
        if rows.len().saturating_add(item_rows.len()) > limit {
            if used == 0 {
                let room = limit.saturating_sub(rows.len());
                rows.extend(item_rows.into_iter().take(room));
                if remaining > 1 && rows.len() < MAX_ROWS {
                    rows.push(format!("  … {} more", remaining.saturating_sub(1)));
                }
            } else {
                rows.push(format!("  … {remaining} more"));
            }
            break;
        }
        rows.extend(item_rows);
        used += 1;
    }
    if rows.len() < 2 {
        rows.resize(2, String::new());
    }
    rows.truncate(MAX_ROWS);
    rows
}

pub(crate) fn approval_detail_view_rows(
    details: &[String],
    width: u16,
    offset: usize,
) -> Vec<String> {
    const MAX_ROWS: usize = 6;
    let width = width.max(1);
    let inner = width.saturating_sub(COMPOSER_PROMPT_CELLS).max(1);
    let mut detail_rows = details
        .iter()
        .flat_map(|detail| detail.lines())
        .flat_map(|line| wrapped_display_rows(line, inner))
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>();
    if detail_rows.is_empty() {
        detail_rows.push("  No additional details are available.".to_string());
    }
    let offset = offset.min(detail_rows.len().saturating_sub(1));
    let mut rows = vec!["> Approval details".to_string()];
    let capacity = MAX_ROWS.saturating_sub(rows.len());
    rows.extend(detail_rows.iter().skip(offset).take(capacity).cloned());
    let remaining = detail_rows.len().saturating_sub(offset + capacity);
    if remaining > 0 {
        if let Some(last) = rows.last_mut() {
            *last = format!("  … {remaining} more rows · Down to inspect");
        }
    }
    if rows.len() < 2 {
        rows.resize(2, String::new());
    }
    rows.truncate(MAX_ROWS);
    rows
}

pub(crate) fn waiting_composer_rows(
    waiting: WaitingKind,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    consent_directory: Option<&str>,
    width: u16,
) -> Option<(Vec<String>, bool)> {
    match waiting {
        WaitingKind::Approval => {
            let req = pending_approval?;
            let prompt = approval_prompt(&req.call, &req.context);
            if req.details_open {
                return Some((
                    approval_detail_view_rows(&req.context.details, width, req.detail_offset),
                    false,
                ));
            }
            let mut extra = if req.call.tool_name == "approve_plan" {
                req.context.details.clone()
            } else {
                Vec::new()
            };
            if extra.is_empty() && !prompt.subject.is_empty() {
                extra.push(prompt.subject);
            }
            if req.call.tool_name != "approve_plan" {
                if let Some(location) =
                    usable_approval_location(prompt.location.clone(), &req.context.target_scope)
                {
                    extra.push(format!("in {location}"));
                }
                let risk = compact_approval_risk(&req.context.permission_and_risk);
                if !risk.is_empty() {
                    extra.push(format!("Risk: {risk}"));
                }
            }
            if let Some(error) = &req.error {
                extra.push(format!("Input error: {error}"));
            }
            Some((overlay_visual_rows(&prompt.statement, &extra, width), false))
        }
        WaitingKind::Question => Some(question_composer_rows(pending_question?, width)),
        WaitingKind::Workspace => {
            let directory = consent_directory?;
            Some((
                overlay_visual_rows("Work in this directory", &[directory.to_string()], width),
                false,
            ))
        }
        WaitingKind::None => None,
    }
}

pub(crate) const CHANNEL_DOT: &str = "● ";
pub(crate) const RESULT_DOT: &str = "· ";

#[derive(Clone, Copy)]
pub(crate) enum ChannelInk {
    User,
    Assistant,
    Thought,
    ToolCall,
    ToolResult,
    System,
    Failure,
}

pub(crate) fn channel_ink(kind: ActivityKind) -> ChannelInk {
    match kind {
        ActivityKind::User => ChannelInk::User,
        ActivityKind::Assistant => ChannelInk::Assistant,
        ActivityKind::Thinking | ActivityKind::Plan => ChannelInk::Thought,
        ActivityKind::Tool | ActivityKind::Approval | ActivityKind::Question => {
            ChannelInk::ToolCall
        }
        ActivityKind::Failure | ActivityKind::Cancellation => ChannelInk::Failure,
        ActivityKind::Compression | ActivityKind::Reconcile | ActivityKind::System => {
            ChannelInk::System
        }
    }
}

pub(crate) fn ink_color(ink: ChannelInk) -> Color {
    match ink {
        ChannelInk::User => Color::Rgb(122, 158, 168),
        ChannelInk::Assistant => Color::Rgb(148, 156, 142),
        ChannelInk::Thought => Color::Rgb(108, 108, 112),
        ChannelInk::ToolCall => Color::Rgb(168, 148, 112),
        ChannelInk::ToolResult => Color::Rgb(118, 122, 130),
        ChannelInk::System => Color::Rgb(128, 132, 128),
        ChannelInk::Failure => Color::Rgb(158, 118, 118),
    }
}

pub(crate) fn ink_style(ink: ChannelInk, no_color: bool) -> Style {
    if no_color {
        match ink {
            ChannelInk::Thought => Style::default().add_modifier(Modifier::ITALIC),
            ChannelInk::Failure => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default(),
        }
    } else {
        let style = Style::default().fg(ink_color(ink));
        match ink {
            ChannelInk::Thought => style.add_modifier(Modifier::ITALIC),
            _ => style,
        }
    }
}

pub(crate) fn muted_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Rgb(108, 108, 112))
    }
}

pub(crate) fn role_style(kind: ActivityKind, no_color: bool) -> Style {
    ink_style(channel_ink(kind), no_color)
}

pub(crate) fn speech_body_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Rgb(168, 168, 164))
    }
}

pub(crate) fn thought_body_style(no_color: bool) -> Style {
    ink_style(ChannelInk::Thought, no_color)
}

pub(crate) fn fold_prefix(entry: &ActivityEntry) -> &'static str {
    if entry.kind == ActivityKind::Thinking {
        return "";
    }
    if entry.folded && !entry.body.is_empty() && !tool_is_live(entry) {
        "› "
    } else {
        ""
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptLive {
    pub(crate) elapsed: Duration,
    pub(crate) tokens: Option<String>,
    pub(crate) tick: u128,
}

pub(crate) fn tool_phase(title: &str) -> &str {
    title
        .split_once(' ')
        .map(|(_, rest)| {
            rest.split_once(" · ")
                .map(|(phase, _)| phase)
                .unwrap_or(rest)
        })
        .unwrap_or("")
}

pub(crate) fn tool_is_live(entry: &ActivityEntry) -> bool {
    entry.kind == ActivityKind::Tool && tool_phase(&entry.title) == "running"
}

pub(crate) fn quiet_tool_title(title: &str) -> String {
    let Some((name, rest)) = title.split_once(' ') else {
        return title.to_string();
    };
    let (phase, remainder) = rest.split_once(" · ").unwrap_or((rest, ""));
    match phase {
        "running" | "requested" | "ok" => {
            if remainder.is_empty() {
                name.to_string()
            } else {
                format!("{name}  {remainder}")
            }
        }
        _ => title.to_string(),
    }
}

pub(crate) fn thought_row_title(entry: &ActivityEntry, live: Option<&TranscriptLive>) -> String {
    if let Some(started) = entry.live_since {
        thought_header_title(
            started.elapsed(),
            live.and_then(|value| value.tokens.as_deref()),
        )
    } else if entry.title.starts_with("Thought") {
        entry.title.clone()
    } else if entry.title.is_empty() || is_legacy_thought_title(&entry.title) {
        live.map(|value| thought_header_title(value.elapsed, value.tokens.as_deref()))
            .unwrap_or_else(|| "Thought".to_string())
    } else {
        entry.title.clone()
    }
}

pub(crate) fn header_body_style(kind: ActivityKind, rest: &str, no_color: bool) -> Style {
    if matches!(kind, ActivityKind::Thinking | ActivityKind::Plan) {
        thought_body_style(no_color)
    } else if matches!(kind, ActivityKind::Assistant | ActivityKind::User) {
        speech_body_style(no_color)
    } else if kind == ActivityKind::Tool {
        tool_status_style(kind, rest, no_color)
    } else {
        ink_style(channel_ink(kind), no_color)
    }
}

pub(crate) fn dotted_header_lines(
    kind: ActivityKind,
    rest: &str,
    fold: &str,
    width: u16,
    no_color: bool,
) -> Vec<Line<'static>> {
    let fold_width = unicode_display_width(fold);
    let dot_width = unicode_display_width(CHANNEL_DOT);
    let inner = usize::from(width.max(1))
        .saturating_sub(fold_width)
        .saturating_sub(dot_width)
        .max(1);
    if rest.is_empty() {
        let mut spans = Vec::new();
        if !fold.is_empty() {
            spans.push(Span::styled(fold.to_string(), muted_style(no_color)));
        }
        spans.push(Span::styled(
            CHANNEL_DOT.to_string(),
            ink_style(channel_ink(kind), no_color),
        ));
        return vec![Line::from(spans)];
    }
    wrapped_display_rows(rest, u16::try_from(inner).unwrap_or(u16::MAX))
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            if index == 0 {
                let mut spans = Vec::new();
                if !fold.is_empty() {
                    spans.push(Span::styled(fold.to_string(), muted_style(no_color)));
                }
                spans.push(Span::styled(
                    CHANNEL_DOT.to_string(),
                    ink_style(channel_ink(kind), no_color),
                ));
                spans.push(Span::styled(row, header_body_style(kind, rest, no_color)));
                Line::from(spans)
            } else {
                Line::from(Span::styled(
                    format!("{}{}{row}", fold, " ".repeat(dot_width)),
                    header_body_style(kind, rest, no_color),
                ))
            }
        })
        .collect()
}

pub(crate) fn dotted_result_lines(
    body: &str,
    width: u16,
    ink: ChannelInk,
    no_color: bool,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    let style = ink_style(ink, no_color);
    body.lines()
        .flat_map(|line| {
            wrapped_display_rows(line, inner)
                .into_iter()
                .map(|row| {
                    Line::from(vec![
                        Span::styled(RESULT_DOT.to_string(), style),
                        Span::styled(row, style),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn visual_channel(kind: ActivityKind) -> u8 {
    match kind {
        ActivityKind::User => 0,
        ActivityKind::Assistant => 1,
        ActivityKind::Thinking | ActivityKind::Plan => 2,
        ActivityKind::Tool | ActivityKind::Approval => 3,
        _ => 4,
    }
}

pub(crate) fn tool_status_style(kind: ActivityKind, rest: &str, no_color: bool) -> Style {
    if kind != ActivityKind::Tool {
        return muted_style(no_color);
    }
    if rest.contains(" failed") {
        ink_style(ChannelInk::Failure, no_color)
    } else if rest.contains(" running") || rest.contains(" requested") {
        ink_style(ChannelInk::ToolCall, no_color)
    } else if rest.contains(" ok") {
        ink_style(ChannelInk::Assistant, no_color)
    } else {
        ink_style(ChannelInk::ToolCall, no_color)
    }
}

pub(crate) fn branch_style(branch: &str, no_color: bool) -> Style {
    if no_color {
        return Style::default().add_modifier(Modifier::BOLD);
    }
    let detached = branch == "-"
        || branch == "HEAD"
        || (branch.len() >= 7 && branch.chars().all(|ch| ch.is_ascii_hexdigit()));
    if detached {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }
}

pub(crate) fn model_style(no_color: bool) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

pub(crate) fn context_style(context: &str, no_color: bool) -> Style {
    if no_color {
        return Style::default();
    }
    let percent = crate::context::snapshot::occupancy_percent_from_label(context);
    if percent >= 90 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if percent >= 70 {
        Style::default().fg(Color::Yellow)
    } else {
        muted_style(false)
    }
}

pub(crate) fn header_line(chrome: &TuiChrome, width: u16, no_color: bool) -> Line<'static> {
    let width = usize::from(width.max(1));
    let mut folder = chrome.folder.clone();
    let mut branch = chrome.branch.clone();
    let mut model = chrome.model.clone();
    let mut context = chrome.context.clone();
    let mut left_width = unicode_display_width(&folder)
        .saturating_add(2)
        .saturating_add(unicode_display_width(&branch));
    let mut right_width = unicode_display_width(&model)
        .saturating_add(2)
        .saturating_add(unicode_display_width(&context));
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        context = crate::context::snapshot::abbreviate_occupancy_indicator(&context);
        right_width = unicode_display_width(&model)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&context));
    }
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        folder = truncate_display_cells(
            &folder,
            22.min(width.saturating_sub(right_width).saturating_sub(3)),
        );
        left_width = unicode_display_width(&folder)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&branch));
    }
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        branch = truncate_display_cells(
            &branch,
            16.min(
                width
                    .saturating_sub(right_width)
                    .saturating_sub(unicode_display_width(&folder).saturating_add(3)),
            ),
        );
        left_width = unicode_display_width(&folder)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&branch));
    }
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        model = truncate_display_cells(
            &model,
            width
                .saturating_sub(left_width)
                .saturating_sub(unicode_display_width(&context).saturating_add(3))
                .max(4),
        );
        right_width = unicode_display_width(&model)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&context));
    }
    let used = left_width.saturating_add(right_width);
    let pad = width.saturating_sub(used);
    let mut spans = vec![
        Span::styled(folder, muted_style(no_color)),
        Span::raw("  "),
        Span::styled(branch, branch_style(&chrome.branch, no_color)),
    ];
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    } else if width > left_width {
        spans.push(Span::raw(" "));
    }
    if width > left_width {
        spans.push(Span::styled(model, model_style(no_color)));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            context.clone(),
            context_style(&context, no_color),
        ));
    }
    Line::from(spans)
}

pub(crate) fn waiting_keys(waiting: WaitingKind) -> &'static str {
    match waiting {
        WaitingKind::Approval => "Enter deny · select Approve once then Enter · Esc deny",
        WaitingKind::Workspace => "Enter decline · select Allow then Enter · Esc decline",
        WaitingKind::Question => "Up/Down select · Enter choose · Esc interrupt operation",
        WaitingKind::None => "",
    }
}

pub(crate) fn agent_mode_label(
    waiting: WaitingKind,
    worker_mode: Option<InteractiveAgentMode>,
) -> &'static str {
    match waiting {
        WaitingKind::Approval => "WAITING APPROVAL",
        WaitingKind::Question => "WAITING QUESTION",
        WaitingKind::Workspace => "WAITING PERMISSION",
        WaitingKind::None => worker_mode
            .map(InteractiveAgentMode::as_str)
            .unwrap_or("idle"),
    }
}

/// The Codex-style update box: rounded borders sized to the widest line by
/// display width.
fn update_box_lines(update: &StartupUpdate, no_color: bool) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let (headline, command) = if no_color {
        (bold, Style::default())
    } else {
        (bold.fg(Color::Yellow), Style::default().fg(Color::Cyan))
    };
    let rows: Vec<Vec<Span<'static>>> = vec![
        vec![
            Span::styled("✨ Update available!", headline),
            Span::raw(format!(" {} -> {}", update.current, update.latest)),
        ],
        vec![
            Span::raw("Run "),
            Span::styled("nib update", command),
            Span::raw(" to update."),
        ],
        Vec::new(),
        vec![Span::raw("See full release notes:")],
        vec![Span::styled(update.release_notes.clone(), command)],
    ];
    let width = |row: &[Span<'static>]| {
        row.iter()
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>()
    };
    let inner = rows.iter().map(|row| width(row)).max().unwrap_or(0);
    let border = muted_style(no_color);
    let rule = "─".repeat(inner + 2);
    let mut lines = vec![Line::from(Span::styled(format!("╭{rule}╮"), border))];
    for row in rows {
        let padding = " ".repeat(inner - width(&row) + 1);
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(row);
        spans.push(Span::raw(padding));
        spans.push(Span::styled("│", border));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(format!("╰{rule}╯"), border)));
    lines
}

pub(crate) fn empty_state_lines(welcome: &StartupWelcome, no_color: bool) -> Vec<Line<'static>> {
    let muted = muted_style(no_color);
    let title = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    if let Some(update) = &welcome.update {
        lines.extend(update_box_lines(update, no_color));
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from(vec![
            Span::styled("  >_ ", muted),
            Span::styled("nib", title),
            Span::styled(format!(" (v{})", welcome.version), muted),
        ]),
        Line::from(Span::styled(
            format!("     {}", welcome.working_directory),
            muted,
        )),
    ]);
    let hints = [
        Line::from(""),
        Line::from(Span::styled("Session", title)),
        Line::from(Span::styled("/new            new session", muted)),
        Line::from(Span::styled(
            "/session        switch or resume a session",
            muted,
        )),
        Line::from(Span::styled(
            "/worktree       work in a separate Git worktree",
            muted,
        )),
        Line::from(Span::styled(
            "/land           merge or discard the worktree's work",
            muted,
        )),
        Line::from(""),
        Line::from(Span::styled("Keys", title)),
        Line::from(Span::styled(
            format!("Keymap {:?} · /keymap to customize", welcome.keymap.preset),
            muted,
        )),
        Line::from(Span::styled(
            format!(
                "{} send · {} newline",
                welcome
                    .keymap
                    .bindings("composer", crate::config::keymap::KeyAction::Send)
                    .iter()
                    .map(|binding| crate::config::keymap::binding_label(binding))
                    .collect::<Vec<_>>()
                    .join("/"),
                welcome
                    .keymap
                    .bindings("composer", crate::config::keymap::KeyAction::Newline)
                    .iter()
                    .map(|binding| crate::config::keymap::binding_label(binding))
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            muted,
        )),
        Line::from(Span::styled(
            format!(
                "{} cancel · {} twice to quit",
                welcome
                    .keymap
                    .bindings("composer", crate::config::keymap::KeyAction::Cancel)
                    .iter()
                    .map(|binding| crate::config::keymap::binding_label(binding))
                    .collect::<Vec<_>>()
                    .join("/"),
                welcome
                    .keymap
                    .bindings("composer", crate::config::keymap::KeyAction::Quit)
                    .iter()
                    .map(|binding| crate::config::keymap::binding_label(binding))
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            muted,
        )),
        Line::from(Span::styled(
            "/ commands · @ files · completion consumes Tab",
            muted,
        )),
        Line::from(Span::styled("drag            copy chat on release", muted)),
        Line::from(Span::styled("Y / N           approve or deny", muted)),
    ];
    // The hints line up with the header, two columns in.
    lines.extend(hints.into_iter().map(|mut line| {
        if line.width() > 0 {
            line.spans.insert(0, Span::raw("  "));
        }
        line
    }));
    lines
}

pub(crate) fn footer_line(
    chrome: &TuiChrome,
    viewport: &TranscriptViewport,
    queued: usize,
    waiting: WaitingKind,
    band_hint: Option<&str>,
    selecting: bool,
) -> String {
    if waiting == WaitingKind::Question {
        return band_hint
            .unwrap_or_else(|| waiting_keys(waiting))
            .to_string();
    }
    // Only a level the user chose (read-only, full-access) is shown next to
    // the mode, so key hints fit narrow terminals; the default level and
    // platform limits such as a missing bwrap are in /status, /permissions
    // and nib doctor (T080 phase 6).
    let mut hint = if chrome
        .sandbox
        .starts_with(crate::sandbox::SandboxLevel::WorkspaceWrite.as_str())
    {
        format!("mode {} · {}", chrome.approval, chrome.agent_mode)
    } else {
        format!(
            "mode {} · {} · {}",
            chrome.approval, chrome.sandbox, chrome.agent_mode
        )
    };
    if waiting != WaitingKind::None {
        hint = format!("{hint} · {}", waiting_keys(waiting));
    } else if selecting {
        let binding = chrome
            .keymap
            .bindings(
                "transcript",
                crate::config::keymap::KeyAction::CopySelection,
            )
            .first()
            .map(|b| crate::config::keymap::binding_label(b))
            .unwrap_or_else(|| "unbound".to_string());
        hint = format!("{hint} · {binding} copy · Esc clear");
    } else if let Some(band) = band_hint {
        hint = format!("{hint} · {band}");
    }
    if queued > 0 {
        hint = format!("queue {queued} · {hint}");
    }
    if !viewport.is_pinned_to_tail() {
        let binding = chrome
            .keymap
            .bindings("transcript", crate::config::keymap::KeyAction::FollowTail)
            .first()
            .map(|b| crate::config::keymap::binding_label(b))
            .unwrap_or_else(|| "unbound".to_string());
        hint.push_str(&format!(" · {binding} follow"));
    }
    // Presence is informational, so it comes last and a narrow terminal
    // clips it before any key hint.
    if let Some(others) = crate::session::presence::other_sessions_label(chrome.other_sessions) {
        hint = format!("{hint} · {others}");
    }
    hint
}

pub(crate) fn spinner_glyph(tick: u128, no_color: bool) -> char {
    if no_color {
        SPINNER_ASCII[tick as usize % SPINNER_ASCII.len()]
    } else {
        SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()]
    }
}

pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let remain = secs % 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{remain:02}s")
    } else {
        format!("{remain}s")
    }
}

pub(crate) fn meter_status_label(waiting: WaitingKind, lifecycle: &str) -> String {
    match waiting {
        WaitingKind::Approval => "WAITING APPROVAL".to_string(),
        WaitingKind::Question => "WAITING QUESTION".to_string(),
        WaitingKind::Workspace => "WAITING PERMISSION".to_string(),
        WaitingKind::None => lifecycle.to_string(),
    }
}

pub(crate) fn render_waiting_meter(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    meter: &WaitingMeter,
    no_color: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = waiting_meter_text(meter, usize::from(area.width), no_color);
    let style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

pub(crate) fn waiting_meter_text(meter: &WaitingMeter, width: usize, no_color: bool) -> String {
    const STARS: [char; 4] = ['✽', '✻', '✼', '✶'];
    let spin = if no_color {
        spinner_glyph(meter.tick / 125, true)
    } else {
        STARS[((meter.tick / 125) % STARS.len() as u128) as usize]
    };
    let (phrase, short) = match meter.status.as_str() {
        "WAITING APPROVAL" => ("Waiting for approval", "Approval"),
        "WAITING QUESTION" | "awaiting you" => ("Waiting for your answer", "Answer"),
        "WAITING PERMISSION" => ("Waiting for permission", "Permission"),
        "planning" => ("Planning", "Planning"),
        "thinking" => ("Thinking", "Thinking"),
        "compression" | "compressing" => ("Compressing", "Compressing"),
        "reconciling" | "reconciliation" => ("Reconciling", "Reconciling"),
        "cancelling" => ("Cancelling", "Cancelling"),
        "cancelled" => ("Cancelled", "Cancelled"),
        "failed" => ("Failed", "Failed"),
        "completed" => ("Completed", "Completed"),
        "running" => ("Flowing", "Flowing"),
        _ => ("Working", "Working"),
    };
    let elapsed = format_elapsed(meter.elapsed);
    let tokens = truncate_display_cells(&control_safe_text(&meter.tokens, false), 24);
    for text in [
        format!("{spin} {phrase}… ({elapsed} · ↓ {tokens} tokens)"),
        format!("{spin} {short}… ({elapsed} · ↓ {tokens})"),
        format!("{spin} {short}… ({elapsed})"),
        format!("{spin} {short}…"),
    ] {
        if unicode_display_width(&text) <= width {
            return text;
        }
    }
    truncate_display_cells(&format!("{spin} {short}…"), width)
}

/// Advertise only actions owned by the currently visible interaction context.
pub(crate) fn keymap_context_hint(
    keymap: &crate::config::keymap::TuiConfig,
    layer: InteractionLayer,
    focus: TuiFocus,
    running: bool,
    workspace_consent: bool,
) -> String {
    let mut hints = vec![format!("{:?}", keymap.preset)];
    if workspace_consent || !matches!(layer, InteractionLayer::Composer) {
        return hints.join(" · ");
    }
    let (context, actions): (&str, &[(KeyAction, &str)]) = match (focus, running) {
        (TuiFocus::Transcript, _) => (
            "transcript",
            &[
                (KeyAction::Focus, "focus"),
                (KeyAction::CopySelection, "copy selection"),
            ],
        ),
        (TuiFocus::Composer, true) => (
            "running",
            &[(KeyAction::Steer, "steer"), (KeyAction::Queue, "queue")],
        ),
        (TuiFocus::Composer, false) => (
            "composer",
            &[
                (KeyAction::Send, "send"),
                (KeyAction::Editor, "editor"),
                (KeyAction::Copy, "copy"),
            ],
        ),
    };
    for (action, label) in actions {
        let bindings = keymap.bindings(context, *action);
        if !bindings.is_empty() {
            let keys = bindings
                .iter()
                .map(|binding| crate::config::keymap::binding_label(binding))
                .collect::<Vec<_>>()
                .join("/");
            hints.push(format!("{keys} {label}"));
        }
    }
    hints.join(" · ")
}
