//! Markdown and fenced-code rendering for TUI speech blocks.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const SPEECH_INDENT: &str = "  ";

pub(super) fn render_markdown(
    source: &str,
    width: u16,
    indent: &str,
    no_color: bool,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut renderer = MarkdownRenderer::new(width, indent, no_color);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    for event in Parser::new_ext(source, options) {
        renderer.handle(event);
    }
    renderer.finish()
}

pub(super) fn speech_indent() -> &'static str {
    SPEECH_INDENT
}

struct MarkdownRenderer {
    width: usize,
    indent: String,
    no_color: bool,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListState>,
    quote_depth: usize,
    pending_link: Option<String>,
    code_lang: Option<String>,
    code_buf: String,
    block_prefix: String,
}

struct ListState {
    next: Option<u64>,
}

impl MarkdownRenderer {
    fn new(width: usize, indent: &str, no_color: bool) -> Self {
        Self {
            width,
            indent: indent.to_string(),
            no_color,
            lines: Vec::new(),
            spans: Vec::new(),
            style_stack: vec![body_style(no_color)],
            list_stack: Vec::new(),
            quote_depth: 0,
            pending_link: None,
            code_lang: None,
            code_buf: String::new(),
            block_prefix: String::new(),
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack
            .last()
            .copied()
            .unwrap_or_else(|| body_style(self.no_color))
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        if self.code_lang.is_some() {
            match event {
                Event::Text(text) | Event::Code(text) => self.code_buf.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.flush_code_block(),
                Event::SoftBreak | Event::HardBreak => self.code_buf.push('\n'),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => self.push_inline_code(&code),
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_paragraph();
                self.push_text("───");
                self.flush_line();
            }
            Event::TaskListMarker(checked) => {
                self.push_text(if checked { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_prefix = self.flow_prefix(),
            Tag::Heading { level, .. } => {
                self.flush_paragraph();
                self.block_prefix = self.flow_prefix();
                self.push_style(heading_style(level, self.no_color));
                self.push_text(&format!("{} ", heading_marker(level)));
            }
            Tag::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush_paragraph();
                self.code_lang = Some(match kind {
                    CodeBlockKind::Fenced(lang) => lang.trim().to_string(),
                    CodeBlockKind::Indented => String::new(),
                });
                self.code_buf.clear();
            }
            Tag::List(start) => {
                self.flush_paragraph();
                self.list_stack.push(ListState { next: start });
            }
            Tag::Item => {
                self.flush_paragraph();
                self.block_prefix = self.flow_prefix();
                match self.list_stack.last_mut() {
                    Some(ListState { next: Some(value) }) => {
                        let marker = format!("{value}. ");
                        *value = value.saturating_add(1);
                        self.push_text(&marker);
                    }
                    _ => self.push_text("• "),
                }
            }
            Tag::Emphasis => self.push_style(self.current_style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(self.current_style().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(self.current_style().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.pending_link = Some(dest_url.to_string());
                self.push_style(link_style(self.no_color));
            }
            Tag::Image { dest_url, .. } => {
                self.push_text("[image ");
                self.push_text(&dest_url);
                self.push_text("]");
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_paragraph(),
            TagEnd::Heading(_) => {
                self.flush_paragraph();
                self.pop_style();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush_paragraph(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                if let Some(url) = self.pending_link.take() {
                    let shown = self
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    if !url.is_empty() && shown != url {
                        self.pop_style();
                        self.push_style(muted_style(self.no_color));
                        self.push_text(&format!(" ({url})"));
                    }
                }
                self.pop_style();
            }
            TagEnd::CodeBlock => self.flush_code_block(),
            _ => {}
        }
    }

    fn flow_prefix(&self) -> String {
        let mut prefix = self.indent.clone();
        for _ in 0..self.quote_depth {
            prefix.push_str("│ ");
        }
        let extra = self.list_stack.len().saturating_mul(2);
        prefix.push_str(&" ".repeat(extra));
        prefix
    }

    fn push_inline_code(&mut self, code: &str) {
        let style = inline_code_style(self.no_color);
        if self.no_color {
            self.push_span(Span::styled(format!("`{code}`"), style));
        } else {
            self.push_span(Span::styled(code.to_string(), style));
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_span(Span::styled(text.to_string(), self.current_style()));
    }

    fn push_span(&mut self, span: Span<'static>) {
        if span.content.is_empty() {
            return;
        }
        self.spans.push(span);
    }

    fn flush_paragraph(&mut self) {
        self.flush_line();
    }

    fn flush_line(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let prefix = if self.block_prefix.is_empty() {
            self.indent.clone()
        } else {
            self.block_prefix.clone()
        };
        self.lines
            .extend(wrap_prefixed_spans(spans, self.width, &prefix));
        self.block_prefix = self.flow_prefix();
        if !self.block_prefix.is_empty() {
            // Continuation lines of a wrapped list item stay indented past the marker.
            let continuation = self.list_stack.len().saturating_mul(2).saturating_add(
                if self.list_stack.last().is_some() {
                    2
                } else {
                    0
                },
            );
            if continuation > 0 {
                self.block_prefix = format!("{}{}", self.indent, " ".repeat(continuation));
                for _ in 0..self.quote_depth {
                    self.block_prefix.push_str("│ ");
                }
            }
        }
    }

    fn flush_code_block(&mut self) {
        let lang = self.code_lang.take().unwrap_or_default();
        let body = std::mem::take(&mut self.code_buf);
        let label = if lang.is_empty() {
            "code".to_string()
        } else {
            lang.clone()
        };
        self.lines.push(Line::from(vec![
            Span::raw(self.indent.clone()),
            Span::styled(label, code_label_style(self.no_color)),
        ]));
        let code_indent = format!("{}  ", self.indent);
        if body.is_empty() {
            return;
        }
        for logical in body.split('\n') {
            if logical.is_empty() {
                self.lines.push(Line::from(Span::raw(code_indent.clone())));
                continue;
            }
            let highlighted = highlight_code_line(logical, &lang, self.no_color);
            self.lines
                .extend(wrap_prefixed_spans(highlighted, self.width, &code_indent));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_paragraph();
        if self.code_lang.is_some() {
            self.flush_code_block();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::from(Span::raw(self.indent.clone())));
        }
        self.lines
    }
}

fn wrap_prefixed_spans(
    spans: Vec<Span<'static>>,
    width: usize,
    prefix: &str,
) -> Vec<Line<'static>> {
    let prefix_width = unicode_width(prefix);
    let inner = width.saturating_sub(prefix_width).max(1);
    wrap_spans(spans, inner)
        .into_iter()
        .map(|line| {
            let mut out = vec![Span::raw(prefix.to_string())];
            out.extend(line.spans);
            Line::from(out)
        })
        .collect()
}

fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for span in spans {
        let style = span.style;
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if row_width.saturating_add(grapheme_width) > width && row_width > 0 {
                rows.push(Vec::new());
                row_width = 0;
            }
            let row = rows.last_mut().expect("row");
            match row.last_mut() {
                Some(last) if last.style == style => {
                    last.content.to_mut().push_str(grapheme);
                }
                _ => row.push(Span::styled(grapheme.to_string(), style)),
            }
            row_width = row_width.saturating_add(grapheme_width);
        }
    }
    if rows.len() == 1 && rows[0].is_empty() {
        return Vec::new();
    }
    rows.into_iter().map(Line::from).collect()
}

fn unicode_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn body_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::White)
    }
}

fn muted_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn heading_style(level: HeadingLevel, no_color: bool) -> Style {
    let style = Style::default().add_modifier(Modifier::BOLD);
    if no_color {
        return style;
    }
    match level {
        HeadingLevel::H1 => style.fg(Color::White),
        HeadingLevel::H2 => style.fg(Color::Cyan),
        _ => style.fg(Color::DarkGray),
    }
}

fn heading_marker(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "#",
        HeadingLevel::H2 => "##",
        HeadingLevel::H3 => "###",
        HeadingLevel::H4 => "####",
        HeadingLevel::H5 => "#####",
        HeadingLevel::H6 => "######",
    }
}

fn inline_code_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default()
            .fg(Color::Magenta)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD)
    }
}

fn link_style(no_color: bool) -> Style {
    let style = Style::default().add_modifier(Modifier::UNDERLINED);
    if no_color {
        style
    } else {
        style.fg(Color::Blue)
    }
}

fn code_label_style(no_color: bool) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::ITALIC)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC)
    }
}

fn code_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn keyword_style(no_color: bool) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

fn string_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Green)
    }
}

fn comment_style(no_color: bool) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::ITALIC)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    }
}

fn number_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Yellow)
    }
}

fn keywords_for(lang: &str) -> &'static [&'static str] {
    match normalize_lang(lang) {
        "rust" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ],
        "python" => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "None", "not", "or", "pass", "raise", "return", "True", "try",
            "while", "with", "yield",
        ],
        "js" | "ts" => &[
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "debugger",
            "default",
            "delete",
            "else",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "import",
            "in",
            "instanceof",
            "let",
            "new",
            "null",
            "return",
            "static",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "var",
            "void",
            "while",
            "yield",
        ],
        "go" => &[
            "break",
            "case",
            "chan",
            "const",
            "continue",
            "default",
            "defer",
            "else",
            "fallthrough",
            "false",
            "for",
            "func",
            "go",
            "goto",
            "if",
            "import",
            "interface",
            "map",
            "nil",
            "package",
            "range",
            "return",
            "select",
            "struct",
            "switch",
            "true",
            "type",
            "var",
        ],
        "bash" | "sh" => &[
            "break", "case", "continue", "do", "done", "elif", "else", "esac", "export", "fi",
            "for", "function", "if", "in", "return", "select", "then", "until", "while",
        ],
        "sql" => &[
            "and", "as", "asc", "by", "case", "create", "delete", "desc", "drop", "else", "end",
            "from", "group", "having", "in", "insert", "into", "join", "limit", "not", "null",
            "on", "or", "order", "select", "set", "table", "then", "update", "values", "where",
        ],
        "toml" | "yaml" => &["true", "false", "null"],
        "json" => &["true", "false", "null"],
        _ => &[],
    }
}

fn normalize_lang(lang: &str) -> &str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => "rust",
        "py" | "python" => "python",
        "js" | "javascript" | "jsx" | "mjs" => "js",
        "ts" | "typescript" | "tsx" => "ts",
        "go" | "golang" => "go",
        "bash" | "zsh" | "shell" | "sh" => "bash",
        "sql" => "sql",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        other => {
            // Keep the original slice when the language is already a lowercase
            // identifier we do not special-case; keyword lookup then returns [].
            let _ = other;
            lang
        }
    }
}

fn line_comment_prefix(lang: &str) -> Option<&'static str> {
    match normalize_lang(lang) {
        "python" | "bash" | "toml" | "yaml" => Some("#"),
        "sql" => Some("--"),
        "json" => None,
        "" => None,
        _ => Some("//"),
    }
}

fn highlight_code_line(line: &str, lang: &str, no_color: bool) -> Vec<Span<'static>> {
    if no_color {
        return vec![Span::styled(line.to_string(), code_style(true))];
    }
    if let Some(prefix) = line_comment_prefix(lang) {
        if let Some(index) = find_unquoted(line, prefix) {
            let mut spans = highlight_code_line(&line[..index], lang, no_color);
            spans.push(Span::styled(
                line[index..].to_string(),
                comment_style(false),
            ));
            return spans;
        }
    }
    let keywords = keywords_for(lang);
    let mut spans = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        if let Some((taken, style, leftover)) = take_string(rest, no_color) {
            spans.push(Span::styled(taken, style));
            rest = leftover;
            continue;
        }
        if rest.starts_with(|ch: char| ch.is_ascii_digit()) {
            let end = rest
                .char_indices()
                .find(|(_, ch)| !ch.is_ascii_alphanumeric() && *ch != '.' && *ch != '_')
                .map(|(index, _)| index)
                .unwrap_or(rest.len());
            spans.push(Span::styled(
                rest[..end].to_string(),
                number_style(no_color),
            ));
            rest = &rest[end..];
            continue;
        }
        if rest.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_') {
            let end = rest
                .char_indices()
                .find(|(_, ch)| !ch.is_ascii_alphanumeric() && *ch != '_')
                .map(|(index, _)| index)
                .unwrap_or(rest.len());
            let word = &rest[..end];
            let style = if keywords.contains(&word) {
                keyword_style(no_color)
            } else {
                code_style(no_color)
            };
            spans.push(Span::styled(word.to_string(), style));
            rest = &rest[end..];
            continue;
        }
        let ch = rest.chars().next().expect("non-empty");
        let len = ch.len_utf8();
        spans.push(Span::styled(rest[..len].to_string(), code_style(no_color)));
        rest = &rest[len..];
    }
    spans
}

fn take_string(input: &str, no_color: bool) -> Option<(String, Style, &str)> {
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' && quote != '`' {
        return None;
    }
    let mut end = 1;
    let bytes = input.as_bytes();
    while end < input.len() {
        if bytes[end] == b'\\' && end + 1 < input.len() {
            end += 2;
            continue;
        }
        if bytes[end] == quote as u8 {
            end += 1;
            break;
        }
        end += 1;
    }
    Some((
        input[..end].to_string(),
        string_style(no_color),
        &input[end..],
    ))
}

fn find_unquoted(line: &str, needle: &str) -> Option<usize> {
    let mut in_string = None;
    let mut escaped = false;
    let chars: Vec<char> = line.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut index = 0usize;
    let mut byte_index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            escaped = false;
            byte_index += ch.len_utf8();
            index += 1;
            continue;
        }
        if in_string.is_some() && ch == '\\' {
            escaped = true;
            byte_index += ch.len_utf8();
            index += 1;
            continue;
        }
        if let Some(quote) = in_string {
            if ch == quote {
                in_string = None;
            }
            byte_index += ch.len_utf8();
            index += 1;
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            byte_index += ch.len_utf8();
            index += 1;
            continue;
        }
        if chars[index..].starts_with(&needle_chars) {
            return Some(byte_index);
        }
        byte_index += ch.len_utf8();
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_headings_lists_emphasis_and_inline_code() {
        let source = "# Title\n\nUse `path` and **bold** plus *italic*.\n\n- one\n- two\n";
        let lines = render_markdown(source, 60, "  ", true);
        let joined = texts(&lines).join("\n");
        assert!(joined.contains("# Title"), "{joined}");
        assert!(joined.contains("`path`"), "{joined}");
        assert!(joined.contains("bold"), "{joined}");
        assert!(joined.contains("• one"), "{joined}");
        assert!(joined.contains("• two"), "{joined}");
        assert!(lines.iter().all(|line| line.spans[0].content == "  "
            || line.spans[0].content.as_ref().starts_with("  ")));
    }

    #[test]
    fn renders_fenced_code_with_language_and_keywords() {
        let source = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n";
        let color = render_markdown(source, 80, "  ", false);
        let joined = texts(&color).join("\n");
        assert!(joined.contains("rust"), "{joined}");
        assert!(joined.contains("fn main()"), "{joined}");
        assert!(joined.contains("println!"), "{joined}");
        assert!(
            color.iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref() == "fn" && span.style.fg == Some(Color::Cyan))
            }),
            "{color:?}"
        );
        let plain = render_markdown(source, 80, "  ", true);
        let plain_text = texts(&plain).join("\n");
        assert!(plain_text.contains("fn main()"), "{plain_text}");
        assert!(plain
            .iter()
            .all(|line| line.spans.iter().all(|span| span.style.fg.is_none())));
    }

    #[test]
    fn wraps_long_prose_without_dropping_characters() {
        let source = "abcdefghijabcdefghijabcdefghij";
        let lines = render_markdown(source, 12, "  ", true);
        let joined: String = texts(&lines).iter().map(|row| row.trim_start()).collect();
        assert_eq!(joined, source);
        assert!(lines.len() >= 3, "{lines:?}");
    }

    #[test]
    fn unclosed_fence_still_shows_code() {
        let source = "```python\nprint(1)";
        let lines = render_markdown(source, 40, "  ", true);
        let joined = texts(&lines).join("\n");
        assert!(
            joined.contains("python") || joined.contains("print(1)"),
            "{joined}"
        );
        assert!(joined.contains("print(1)"), "{joined}");
    }
}
