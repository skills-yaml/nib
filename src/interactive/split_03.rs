//! T043 split.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    User,
    Assistant,
    Thinking,
    Plan,
    Tool,
    Approval,
    Question,
    Compression,
    Reconcile,
    Cancellation,
    Failure,
    System,
}

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub kind: ActivityKind,
    pub title: String,
    pub body: String,
    pub folded: bool,
    /// Runtime-only correlation for a live tool block. It is deliberately kept
    /// out of the rendered text and persisted session projection.
    pub tool_invocation_id: Option<ToolInvocationId>,
    /// Correlates a live progress row with its persisted plan.
    pub plan_id: Option<String>,
    /// Runtime-only start time for an open thought block. Ignored in equality.
    pub live_since: Option<Instant>,
}

impl PartialEq for ActivityEntry {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.title == other.title
            && self.body == other.body
            && self.folded == other.folded
            && self.tool_invocation_id == other.tool_invocation_id
            && self.plan_id == other.plan_id
    }
}

impl Eq for ActivityEntry {}

impl ActivityKind {
    pub fn role_label(self) -> &'static str {
        match self {
            Self::User => "you",
            Self::Assistant => "nib",
            Self::Thinking => "thought",
            Self::Plan => "plan",
            Self::Tool => "tool",
            Self::Approval => "approval",
            Self::Question => "question",
            Self::Compression => "compression",
            Self::Reconcile => "reconcile",
            Self::Cancellation => "cancel",
            Self::Failure => "fail",
            Self::System => "system",
        }
    }
}

impl ActivityEntry {
    pub fn new(kind: ActivityKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: body.into(),
            folded: false,
            tool_invocation_id: None,
            plan_id: None,
            live_since: None,
        }
    }

    pub fn folded(mut self) -> Self {
        self.folded = true;
        self
    }

    pub fn for_tool_invocation(mut self, invocation_id: ToolInvocationId) -> Self {
        self.tool_invocation_id = Some(invocation_id);
        self
    }

    pub fn display_text(&self) -> String {
        match self.kind {
            ActivityKind::Tool => self.tool_display_text(),
            ActivityKind::Thinking => self.thought_display_text(),
            ActivityKind::Plan => self.plan_display_text(),
            ActivityKind::Assistant | ActivityKind::User => self.speech_display_text(),
            _ => self.log_display_text(),
        }
    }

    pub(crate) fn channel_label(&self) -> &'static str {
        match self.kind {
            ActivityKind::Thinking | ActivityKind::Plan => "thought",
            other => other.role_label(),
        }
    }

    pub(crate) fn speech_display_text(&self) -> String {
        let speaker = self.kind.role_label();
        let content = if self.title.is_empty() || self.title == "live" {
            self.body.as_str()
        } else if self.body.is_empty() || self.folded {
            self.title.as_str()
        } else {
            return format!(
                "{speaker}\n{}",
                indent_activity_lines(&format!("{}\n{}", self.title, self.body), "  ")
            );
        };
        if content.is_empty() {
            return speaker.to_string();
        }
        format!("{speaker}\n{}", indent_activity_lines(content, "  "))
    }

    pub(crate) fn thought_display_text(&self) -> String {
        let marker = if self.folded { "▸ " } else { "▾ " };
        let header = format!("{marker}{}", thought_heading(self));
        if self.folded || self.body.is_empty() {
            return header;
        }
        format!("{header}\n{}", indent_activity_lines(&self.body, "┊ "))
    }

    pub(crate) fn plan_display_text(&self) -> String {
        if self.body.is_empty() {
            self.title.clone()
        } else {
            format!(
                "{}\n{}",
                self.title,
                indent_activity_lines(&self.body, "  ")
            )
        }
    }

    pub(crate) fn log_display_text(&self) -> String {
        let label = self.channel_label();
        let header = if self.title.is_empty() {
            if self.body.is_empty() {
                label.to_string()
            } else {
                format!("{label}  {}", self.body)
            }
        } else if self.body.is_empty() || self.folded {
            format!("{label}  {}", self.title)
        } else {
            format!("{label}  {}\n{}", self.title, self.body)
        };
        if self.folded && !self.body.is_empty() {
            format!("› {header}")
        } else {
            header
        }
    }

    pub(crate) fn tool_display_text(&self) -> String {
        let fold = if self.folded && !self.body.is_empty() {
            "› "
        } else {
            ""
        };
        let header = format!("{fold}◆ tool  {}", self.title);
        if self.folded || self.body.is_empty() {
            return header;
        }
        let body = self
            .body
            .lines()
            .map(|line| format!("│ {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{header}\n{body}")
    }

    pub fn copy_text(&self) -> String {
        if self.title.is_empty() || self.title == "live" {
            self.body.clone()
        } else if self.body.is_empty() || self.folded {
            self.title.clone()
        } else {
            format!("{}\n{}", self.title, self.body)
        }
    }

    pub fn render_line(&self) -> String {
        self.display_text()
    }
}

pub(crate) fn indent_activity_lines(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn is_thought_state(state: &str) -> bool {
    matches!(state, "planning" | "inspect_llm" | "build_context")
}

pub(crate) fn is_legacy_thought_title(title: &str) -> bool {
    matches!(
        title,
        "planning" | "inspect llm" | "build context" | "inspect_llm" | "build_context"
    )
}

pub(crate) fn compact_elapsed(elapsed: Duration) -> String {
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

pub(crate) fn meter_token_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim().strip_prefix("tok ").unwrap_or(raw).trim();
    if trimmed.is_empty() || trimmed == "-" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn thought_header_title(elapsed: Duration, token_label: Option<&str>) -> String {
    let time = compact_elapsed(elapsed);
    match token_label
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "-")
    {
        Some(tokens) => format!("Thought for {time}, {tokens} tokens"),
        None => format!("Thought for {time}"),
    }
}

pub(crate) fn thought_heading(entry: &ActivityEntry) -> String {
    if entry.title.starts_with("Thought") {
        entry.title.clone()
    } else if entry.title.is_empty() || is_legacy_thought_title(&entry.title) {
        "Thought".to_string()
    } else {
        entry.title.clone()
    }
}

pub(crate) fn freeze_open_thought(activities: &mut [ActivityEntry]) {
    let Some(entry) = activities.last_mut() else {
        return;
    };
    if entry.kind != ActivityKind::Thinking {
        return;
    }
    if let Some(started) = entry.live_since.take() {
        entry.title = thought_header_title(started.elapsed(), None);
    } else if !entry.title.starts_with("Thought") {
        entry.title = "Thought".to_string();
    }
    entry.folded = true;
}

pub(crate) fn upsert_thought_activity(activities: &mut Vec<ActivityEntry>, _state: &str) {
    if activities
        .last()
        .is_some_and(|entry| entry.kind == ActivityKind::Thinking)
    {
        return;
    }
    activities.push(ActivityEntry {
        kind: ActivityKind::Thinking,
        title: "Thought".to_string(),
        body: String::new(),
        folded: true,
        tool_invocation_id: None,
        plan_id: None,
        live_since: Some(Instant::now()),
    });
}

pub fn classify_composer_submit(worker_active: bool) -> ComposerSubmitKind {
    if worker_active {
        ComposerSubmitKind::QueueNext
    } else {
        ComposerSubmitKind::IdleTurn
    }
}

pub fn steer_hint_message() -> &'static str {
    STEER_HINT
}

pub fn parse_queue_line(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    trimmed
        .strip_prefix("queue:")
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

pub fn parse_steer_line(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    trimmed
        .strip_prefix("steer:")
        .map(str::trim)
        .filter(|text| !text.is_empty())
}
