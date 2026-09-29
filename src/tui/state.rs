//! TUI internals split for T043 module size.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct LiveOutput {
    pub(crate) text: String,
    pub(crate) state: Option<String>,
}

pub(crate) const MAX_LIVE_OUTPUT_BYTES: usize = 1_048_576;
pub(crate) const OMITTED_OUTPUT_MARKER: &str = "[older live output omitted]\n";
#[cfg(test)]
pub(crate) const MAX_SESSION_DETAIL_BYTES: usize = 131_072;
#[cfg(test)]
pub(crate) const MAX_SESSION_DETAIL_ITEMS: usize = 100;
pub(crate) const MAX_SESSION_DETAIL_ITEM_CHARS: usize = 500;
#[cfg(test)]
pub(crate) const MAX_SESSION_DETAIL_ROWS: usize = 400;
#[cfg(test)]
pub(crate) const SESSION_DETAIL_TRUNCATED_MARKER: &str = "\n[session detail truncated]\n";
pub(crate) const MAX_VISIBLE_COMPLETIONS: usize = 7;
pub(crate) const COMPOSER_BORDER_ROWS: u16 = 1;
pub(crate) const COMPOSER_PROMPT_CELLS: u16 = 2;
pub(crate) const CLEAR_DRAFT_CONFIRM: Duration = Duration::from_millis(800);
pub(crate) const QUIT_CONFIRM: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitConfirmAction {
    Arm,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuitArm {
    pub(crate) armed_at: Instant,
    pub(crate) consumer: InteractionLayer,
}

pub(crate) fn quit_confirm_action(
    armed: Option<QuitArm>,
    now: Instant,
    consumer: InteractionLayer,
) -> QuitConfirmAction {
    if armed.is_some_and(|armed| {
        armed.consumer == consumer && now.duration_since(armed.armed_at) <= QUIT_CONFIRM
    }) {
        QuitConfirmAction::Confirm
    } else {
        QuitConfirmAction::Arm
    }
}

pub(crate) fn quit_arm_after_input(armed: Option<QuitArm>, is_control_q: bool) -> Option<QuitArm> {
    is_control_q.then_some(armed).flatten()
}

pub(crate) fn quit_arm_for_consumer(
    armed: Option<QuitArm>,
    consumer: InteractionLayer,
) -> Option<QuitArm> {
    armed.filter(|armed| armed.consumer == consumer)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupWelcome {
    pub(crate) version: String,
    pub(crate) working_directory: String,
    pub(crate) update_notice: Option<String>,
    pub(crate) consent_directory: Option<String>,
    pub(crate) consent_selected: usize,
}

impl StartupWelcome {
    pub(crate) fn new(project_root: &Path, update_notice: Option<String>) -> Self {
        Self {
            version: format!("Nib {}", env!("CARGO_PKG_VERSION")),
            working_directory: crate::interactive::folder_label(project_root),
            update_notice: update_notice
                .map(|notice| notice.strip_prefix("[nib] ").unwrap_or(&notice).to_string()),
            consent_directory: None,
            consent_selected: 1,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self {
            version: format!("Nib {}", env!("CARGO_PKG_VERSION")),
            working_directory: "~/project".to_string(),
            update_notice: None,
            consent_directory: None,
            consent_selected: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TuiFocus {
    #[default]
    Composer,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointerSelection {
    pub(crate) anchor_row: usize,
    pub(crate) anchor_col: usize,
    pub(crate) focus_row: usize,
    pub(crate) focus_col: usize,
    pub(crate) moved: bool,
}

impl PointerSelection {
    pub(crate) fn at(row: usize, col: usize) -> Self {
        Self {
            anchor_row: row,
            anchor_col: col,
            focus_row: row,
            focus_col: col,
            moved: false,
        }
    }

    pub(crate) fn drag_to(&mut self, row: usize, col: usize) {
        if row != self.anchor_row || col != self.anchor_col {
            self.moved = true;
        }
        self.focus_row = row;
        self.focus_col = col;
    }

    pub(crate) fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let start = (self.anchor_row, self.anchor_col);
        let end = (self.focus_row, self.focus_col);
        if start <= end {
            (start, end)
        } else {
            (end, start)
        }
    }

    pub(crate) fn covers_row(&self, row: usize) -> bool {
        if !self.moved {
            return false;
        }
        let ((start_row, _), (end_row, _)) = self.ordered();
        row >= start_row && row <= end_row
    }

    pub(crate) fn is_active(&self) -> bool {
        self.moved
    }
}

pub(crate) fn pointer_selection_is_active(pointer: Option<PointerSelection>) -> bool {
    pointer.is_some_and(|selection| selection.is_active())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptView {
    pub(crate) area: Rect,
    pub(crate) composer_area: Rect,
    pub(crate) top_row: usize,
    pub(crate) plain_rows: Vec<String>,
    pub(crate) owners: Vec<usize>,
    pub(crate) source_session: String,
    pub(crate) source_generation: u64,
    pub(crate) render_cache: Option<TranscriptRenderCache>,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptRenderCache {
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) width: u16,
    pub(crate) no_color: bool,
    pub(crate) live_second: Option<u64>,
    pub(crate) live_tokens: Option<String>,
    pub(crate) rows: Arc<Vec<Line<'static>>>,
    pub(crate) owners: Arc<Vec<usize>>,
    pub(crate) activity_rows: Vec<CachedActivityRows>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedActivityRows {
    pub(crate) source: Arc<ActivityEntry>,
    pub(crate) lines: Arc<Vec<Line<'static>>>,
}

impl TranscriptRenderCache {
    pub(crate) fn matches(
        &self,
        view: &TranscriptView,
        width: u16,
        no_color: bool,
        live: Option<&TranscriptLive>,
    ) -> bool {
        self.session_id == view.source_session
            && self.generation == view.source_generation
            && self.width == width
            && self.no_color == no_color
            && self.live_second == live.map(|state| state.elapsed.as_secs())
            && self.live_tokens.as_deref() == live.and_then(|state| state.tokens.as_deref())
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionDisplayCache {
    pub(crate) session_id: String,
    pub(crate) session: Option<crate::session::Session>,
    pub(crate) token_label: String,
    pub(crate) last_attempt: Option<Instant>,
    pub(crate) pending: Option<mpsc::Receiver<Result<Option<crate::session::Session>, ()>>>,
}

impl SessionDisplayCache {
    pub(crate) fn with_session(session: crate::session::Session) -> Self {
        let token_label = approximate_visible_tokens(Some(&session));
        Self {
            session_id: session.id.clone(),
            session: Some(session),
            token_label,
            last_attempt: Some(Instant::now()),
            pending: None,
        }
    }

    pub(crate) fn set_session(&mut self, session: Option<crate::session::Session>) {
        self.token_label = approximate_visible_tokens(session.as_ref());
        self.session = session;
    }

    pub(crate) fn refresh(&mut self, store: &SessionStore, session_id: &str, now: Instant) {
        if self.session_id != session_id {
            self.session_id = session_id.to_string();
            self.set_session(None);
            self.last_attempt = None;
            self.pending = None;
        }
        if let Some(receiver) = self.pending.as_ref() {
            match receiver.try_recv() {
                Ok(Ok(session)) => {
                    self.set_session(session);
                    self.pending = None;
                }
                Ok(Err(())) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self
            .last_attempt
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
            || self.pending.is_some()
        {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let store = store.clone();
        let session_id = session_id.to_string();
        if std::thread::Builder::new()
            .name("nib-tui-session-display".to_string())
            .spawn(move || {
                let result = store
                    .load_result_with_deadline(
                        &session_id,
                        Instant::now() + Duration::from_millis(250),
                    )
                    .map_err(|_| ());
                let _ = sender.send(result);
            })
            .is_ok()
        {
            self.pending = Some(receiver);
            self.last_attempt = Some(now);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum WaitingKind {
    #[default]
    None,
    Approval,
    Question,
    Workspace,
}

pub(crate) enum InteractionBand<'a> {
    Completion(&'a CompletionMenu),
    Model(&'a PendingModelSelection),
    Sessions {
        switcher: &'a SessionSwitcher,
        active: &'a str,
    },
    History(&'a PendingHistorySearch),
    Approval(&'a TuiApprovalRequest),
    Question(&'a PendingQuestion),
    Workspace {
        directory: &'a str,
        selected: usize,
    },
}

impl InteractionBand<'_> {
    pub(crate) fn row_count(&self) -> usize {
        match self {
            Self::Completion(menu) => menu.suggestions.len(),
            Self::Model(model) => model.selection.available.len().max(1),
            Self::Sessions { switcher, .. } => {
                let rows = switcher.candidates.len().max(1);
                if switcher.confirming {
                    rows.max(2)
                } else if !switcher.exact_id.is_empty() {
                    rows.saturating_add(1)
                } else {
                    rows
                }
            }
            Self::History(search) => search.search.matches.len().max(1).saturating_add(1),
            Self::Approval(request) if request.call.tool_name == "run_terminal" => {
                command_approval_lines(request).len().max(2)
            }
            Self::Approval(_) | Self::Workspace { .. } => 2,
            Self::Question(question) if question.request.options.is_empty() => 1,
            Self::Question(question) => question.request.options.len().saturating_add(1),
        }
    }

    pub(crate) fn footer_hint(&self) -> &'static str {
        match self {
            Self::Completion(_) => "Tab insert · Enter run · Esc close",
            Self::Model(_) => "Enter select · Esc cancel",
            Self::Sessions { switcher, .. } if switcher.confirming => {
                "Y/Enter resume · N/Esc keep current"
            }
            Self::Sessions { .. } => "Enter resume · Esc close",
            Self::History(_) => "Enter restore · Esc close",
            Self::Approval(request) if request.call.tool_name == "run_terminal" => {
                "Press enter to confirm or esc to cancel"
            }
            Self::Approval(_) => "Y/Enter approve once · N deny · Esc deny",
            Self::Question(_) => "Enter / 1-9 answer · Esc skip",
            Self::Workspace { .. } => "Y/Enter allow this directory · N decline",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WaitingMeter {
    pub(crate) job: String,
    pub(crate) step: String,
    pub(crate) elapsed: Duration,
    pub(crate) tokens: String,
    pub(crate) status: String,
    pub(crate) tick: u128,
}

pub(crate) struct SessionLayout {
    pub(crate) header: Rect,
    pub(crate) transcript: Rect,
    pub(crate) meter: Rect,
    pub(crate) composer: Rect,
    pub(crate) completion: Rect,
    pub(crate) footer: Rect,
}

pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub(crate) const SPINNER_ASCII: &[char] = &['|', '/', '-', '\\'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionKeyResult {
    Ignored,
    Consumed,
    Submit,
}

#[derive(Debug, Clone)]
pub(crate) struct ChromeCache {
    pub(crate) generation: u64,
    pub(crate) session_id: String,
    pub(crate) session_revision: Option<u64>,
    pub(crate) origin: String,
    pub(crate) chrome: TuiChrome,
}

impl ChromeCache {
    pub(crate) fn matches(
        &self,
        generation: u64,
        session_id: &str,
        session_revision: Option<u64>,
        origin: &str,
    ) -> bool {
        self.generation == generation
            && self.session_id == session_id
            && self.session_revision == session_revision
            && self.origin == origin
    }
}
pub(crate) const MAX_SWITCHER_CANDIDATES: usize = 100;
pub(crate) const MAX_SWITCHER_EXACT_ID_BYTES: usize = 256;
pub(crate) const AGENT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl LiveOutput {
    pub(crate) fn apply(&mut self, event: StreamEvent, sensitive_values: &[String]) {
        if let StreamEvent::StateTransition { state } = &event {
            self.state = Some(state.clone());
        }
        match display_stream_event_with_sensitive_values(event, sensitive_values) {
            Some(StreamDisplay::Content(content)) => self.push_raw(&content),
            Some(StreamDisplay::Status(status)) => self.push_status(status),
            None => {}
        }
    }

    pub(crate) fn push_status(&mut self, status: String) {
        let status = control_safe_text(&status, true);
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.text.push_str(&status);
        self.text.push('\n');
        self.enforce_bound();
    }

    pub(crate) fn push_raw(&mut self, content: &str) {
        self.text.push_str(&control_safe_text(content, true));
        self.enforce_bound();
    }

    pub(crate) fn enforce_bound(&mut self) {
        if self.text.len() <= MAX_LIVE_OUTPUT_BYTES {
            return;
        }
        let keep = MAX_LIVE_OUTPUT_BYTES.saturating_sub(OMITTED_OUTPUT_MARKER.len());
        let mut start = self.text.len().saturating_sub(keep);
        while start < self.text.len() && !self.text.is_char_boundary(start) {
            start += 1;
        }
        let tail = self.text.split_off(start);
        self.text.clear();
        self.text.push_str(OMITTED_OUTPUT_MARKER);
        self.text.push_str(&tail);
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionDetail {
    pub(crate) text: String,
}

#[cfg(test)]
impl SessionDetail {
    pub(crate) fn new(
        id: &str,
        session: Option<&crate::session::Session>,
        sensitive_values: &[String],
    ) -> Self {
        let mut text = format!("Session: {id}\n");
        if let Some(session) = session {
            text.push_str(&format!("Messages: {}\n", session.messages.len()));
            if let Some(plan) = &session.plan {
                text.push_str(&format!(
                    "Plan: step {}/{}; outcome={}\n",
                    plan.current_step_index.min(plan.steps.len()),
                    plan.steps.len(),
                    plan.outcome.as_deref().unwrap_or("active")
                ));
            }
            text.push('\n');

            let message_start = session
                .messages
                .len()
                .saturating_sub(MAX_SESSION_DETAIL_ITEMS);
            if message_start > 0 {
                text.push_str(&format!("[{message_start} earlier messages omitted]\n\n"));
            }
            for message in &session.messages[message_start..] {
                text.push_str(&format!(
                    "#{} [{}]\n{}\n\n",
                    message.index,
                    message.role,
                    bounded_preview(&message.content, sensitive_values)
                ));
            }

            text.push_str(&format!("Tool calls: {}\n", session.tool_calls.len()));
            let tool_start = session
                .tool_calls
                .len()
                .saturating_sub(MAX_SESSION_DETAIL_ITEMS);
            if tool_start > 0 {
                text.push_str(&format!("[{tool_start} earlier tool calls omitted]\n"));
            }
            for (index, call) in session.tool_calls[tool_start..].iter().enumerate() {
                let status = if call.error.is_some() { "failed" } else { "ok" };
                text.push_str(&format!(
                    "#{} {} ({status})",
                    tool_start + index,
                    call.tool_name.as_deref().unwrap_or("unknown")
                ));
                if let Some(error) = &call.error {
                    text.push_str(&format!(": {}", bounded_preview(error, sensitive_values)));
                }
                text.push('\n');
            }

            text.push_str(&format!("\nLifecycle events: {}\n", session.events.len()));
            let event_start = session
                .events
                .len()
                .saturating_sub(MAX_SESSION_DETAIL_ITEMS);
            if event_start > 0 {
                text.push_str(&format!("[{event_start} earlier events omitted]\n"));
            }
            for event in &session.events[event_start..] {
                let details = if event.kind == "steering_input" {
                    safe_event_fields(&event.details, &["sequence", "source"])
                } else {
                    bounded_preview(&event.details.to_string(), sensitive_values)
                };
                text.push_str(&format!("#{} {}: {}\n", event.index, event.kind, details,));
            }
        } else {
            text.push_str("Session is no longer available.\n");
        }

        text = bounded_public_text(&text, sensitive_values, MAX_SESSION_DETAIL_BYTES, true);
        truncate_session_detail(&mut text);
        truncate_session_detail_rows(&mut text);
        Self { text }
    }
}

#[cfg(test)]
pub(crate) fn bounded_preview(content: &str, sensitive_values: &[String]) -> String {
    let safe = bounded_public_text(
        content,
        sensitive_values,
        MAX_SESSION_DETAIL_ITEM_CHARS.saturating_mul(4),
        true,
    );
    let mut characters = safe.chars();
    let mut preview: String = characters
        .by_ref()
        .take(MAX_SESSION_DETAIL_ITEM_CHARS)
        .collect();
    if characters.next().is_some() {
        preview.push_str("...");
    }
    preview
}

#[cfg(test)]
pub(crate) fn truncate_session_detail(text: &mut String) {
    if text.len() <= MAX_SESSION_DETAIL_BYTES {
        return;
    }
    let mut end = MAX_SESSION_DETAIL_BYTES.saturating_sub(SESSION_DETAIL_TRUNCATED_MARKER.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(SESSION_DETAIL_TRUNCATED_MARKER);
}

#[cfg(test)]
pub(crate) fn truncate_session_detail_rows(text: &mut String) {
    let line_count = text.lines().count();
    if line_count <= MAX_SESSION_DETAIL_ROWS {
        return;
    }
    let keep_from = line_count.saturating_sub(MAX_SESSION_DETAIL_ROWS - 2);
    let tail = text.lines().skip(keep_from).collect::<Vec<_>>().join("\n");
    *text = format!("[{} earlier timeline rows omitted]\n{}\n", keep_from, tail);
    truncate_session_detail(text);
}

#[derive(Debug, Default)]
pub(crate) struct ActiveTimeline {
    pub(crate) session_id: String,
    pub(crate) render_generation: u64,
    pub(crate) active_run_id: Option<String>,
    pub(crate) reconciled_terminal: Option<InteractionTerminalOutcome>,
    pub(crate) reconciled_outcome: Option<String>,
    pub(crate) activities: Vec<ActivityEntry>,
    pub(crate) live: LiveOutput,
    pub(crate) sensitive_values: Vec<String>,
    pub(crate) run_started_at: Option<Instant>,
}

impl ActiveTimeline {
    pub(crate) fn load(store: &SessionStore, session_id: &str) -> io::Result<Self> {
        Self::load_with_session(store, session_id).map(|(timeline, _)| timeline)
    }

    pub(crate) fn load_with_session(
        store: &SessionStore,
        session_id: &str,
    ) -> io::Result<(Self, crate::session::Session)> {
        let session = store
            .load_result(session_id)
            .map_err(|error| {
                io::Error::other(format!("failed to load session {session_id}: {error}"))
            })?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("session {session_id} no longer exists"),
                )
            })?;
        let timeline = Self::from_session(&session, store.public_sensitive_values().to_vec());
        Ok((timeline, session))
    }

    pub(crate) fn from_session(
        session: &crate::session::Session,
        sensitive_values: Vec<String>,
    ) -> Self {
        let activities = project_session_activities(session, &sensitive_values);
        Self {
            session_id: session.id.clone(),
            render_generation: 0,
            active_run_id: None,
            reconciled_terminal: None,
            reconciled_outcome: None,
            activities,
            live: LiveOutput::default(),
            sensitive_values,
            run_started_at: None,
        }
    }

    pub(crate) fn refresh_plan_from_session(&mut self, session: &crate::session::Session) {
        let Some(plan) = session.plan.as_ref().filter(|plan| plan.steps.len() > 1) else {
            return;
        };
        let activity = crate::interactive::plan_activity(plan, &self.sensitive_values);
        if let Some(existing) = self.activities.iter_mut().rev().find(|entry| {
            entry.kind == ActivityKind::Plan && entry.plan_id.as_deref() == Some(plan.id.as_str())
        }) {
            if *existing != activity {
                *existing = activity;
                self.render_generation = self.render_generation.wrapping_add(1);
            }
        } else {
            self.activities.push(activity);
            self.render_generation = self.render_generation.wrapping_add(1);
        }
    }

    pub(crate) fn push_status(&mut self, status: String) {
        self.render_generation = self.render_generation.wrapping_add(1);
        let status =
            bounded_public_text(&status, &self.sensitive_values, MAX_LIVE_OUTPUT_BYTES, true);
        self.live.push_status(status.clone());
        self.activities.push(ActivityEntry::new(
            ActivityKind::System,
            status,
            String::new(),
        ));
    }

    pub(crate) fn push_steering(&mut self, _text: &str, sequence: usize) {
        self.render_generation = self.render_generation.wrapping_add(1);
        self.activities.push(ActivityEntry::new(
            ActivityKind::User,
            format!("steer {sequence}"),
            "instruction persisted for the exact active run",
        ));
        self.live
            .push_status(format!("[steer] accepted for safe boundary #{sequence}"));
    }

    pub(crate) fn apply_event(&mut self, event: StreamEvent) {
        self.render_generation = self.render_generation.wrapping_add(1);
        if let StreamEvent::Reconciled { outcome } = &event {
            if !matches!(outcome.as_str(), "step_completed" | "verification_recovery") {
                if let InteractionReduction::Reconciled { terminal, outcome } = reduce_interaction(
                    &InteractionState::default(),
                    InteractionInput::ReconciledOutcome {
                        outcome,
                        failure: false,
                    },
                ) {
                    self.reconciled_terminal = Some(terminal);
                    self.reconciled_outcome = Some(outcome);
                }
            }
        }
        let duplicate_end = matches!(&event, StreamEvent::End(outcome)
            if self.reconciled_outcome.as_deref() == Some(outcome.as_str()));
        if !duplicate_end {
            if let StreamEvent::End(outcome) = &event {
                if let InteractionReduction::Reconciled { terminal, .. } = reduce_interaction(
                    &InteractionState::default(),
                    InteractionInput::ReconciledOutcome {
                        outcome,
                        failure: false,
                    },
                ) {
                    // Final persistence can fail after a successful reconciliation.
                    // Preserve that failure and prevent queued work from advancing.
                    if terminal != InteractionTerminalOutcome::Completed {
                        self.reconciled_terminal = Some(terminal);
                    }
                }
                self.reconciled_outcome = Some(outcome.clone());
            }
            apply_stream_event(
                &mut self.activities,
                event.clone(),
                &mut self.live.state,
                &self.sensitive_values,
            );
            self.live.apply(event, &self.sensitive_values);
        }
    }

    pub(crate) fn bind_run(&mut self, run_id: Option<String>) {
        if run_id.is_some() {
            self.reconciled_terminal = None;
            self.reconciled_outcome = None;
            if self.active_run_id != run_id || self.run_started_at.is_none() {
                self.run_started_at = Some(Instant::now());
            }
        } else {
            self.run_started_at = None;
        }
        self.active_run_id = run_id;
    }

    #[cfg(test)]
    pub(crate) fn rendered_text(&self) -> String {
        let text = self
            .activities
            .iter()
            .map(ActivityEntry::render_line)
            .collect::<Vec<_>>()
            .join("\n\n");
        if self.live.text.is_empty() {
            return text;
        }
        let separator = if text.ends_with('\n') { "" } else { "\n" };
        format!("{text}{separator}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSwitcher {
    pub(crate) candidates: Vec<InteractiveSessionCandidate>,
    pub(crate) selected: usize,
    pub(crate) omitted: usize,
    pub(crate) confirming: bool,
    pub(crate) exact_id: String,
    pub(crate) error: Option<String>,
}

impl SessionSwitcher {
    pub(crate) fn from_selection(
        selection: InteractiveSessionSelection,
        active_session_id: &str,
    ) -> Self {
        let selected = selection
            .candidates
            .iter()
            .position(|candidate| candidate.id == active_session_id)
            .unwrap_or(0);
        Self {
            candidates: selection.candidates,
            selected,
            omitted: selection.omitted,
            confirming: false,
            exact_id: String::new(),
            error: None,
        }
    }
}

pub(crate) fn load_session_switcher(
    store: &SessionStore,
    active_session_id: &str,
) -> io::Result<SessionSwitcher> {
    let selection =
        interactive_session_selection(store, active_session_id).map_err(io::Error::other)?;
    Ok(SessionSwitcher::from_selection(
        selection,
        active_session_id,
    ))
}

pub struct TuiApprovalRequest {
    pub call: ToolCall,
    pub level: PermissionLevel,
    pub context: ApprovalContext,
    pub selected_option: usize,
    pub typed: String,
    pub(crate) reason_draft: Option<String>,
    pub(crate) details_open: bool,
    pub(crate) detail_offset: usize,
    pub(crate) error: Option<String>,
    pub reply: oneshot::Sender<ApprovalDecision>,
}

pub struct TuiApprovalHandler {
    pub tx: mpsc::Sender<TuiApprovalRequest>,
}

#[async_trait::async_trait]
impl ApprovalHandler for TuiApprovalHandler {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        let context = ApprovalContext::compatibility(call, level);
        self.request(call, level, context).await
    }

    async fn handle_approval_with_context(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.request(call, level, context.clone()).await
    }
}

impl TuiApprovalHandler {
    pub(crate) async fn request(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        context: ApprovalContext,
    ) -> ApprovalDecision {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = TuiApprovalRequest {
            call: call.clone(),
            level,
            context,
            selected_option: usize::from(call.tool_name != "run_terminal"),
            typed: String::new(),
            reason_draft: None,
            details_open: false,
            detail_offset: 0,
            error: None,
            reply: reply_tx,
        };
        let _ = self.tx.send(req);
        reply_rx
            .await
            .unwrap_or_else(|_| ApprovalDecision::denied())
    }
}

pub struct TuiQuestionRequest {
    pub question: String,
    pub proposed_answer: Option<String>,
    pub options: Vec<String>,
    pub reply: oneshot::Sender<crate::agent::QuestionOutcome>,
}

pub struct TuiQuestionHandler {
    pub tx: mpsc::Sender<TuiQuestionRequest>,
}

#[async_trait::async_trait]
impl crate::agent::QuestionHandler for TuiQuestionHandler {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        match self
            .ask_with_context(crate::agent::QuestionRequestContext {
                invocation_id: crate::tools::ToolInvocationId::new(),
                question,
                proposed_answer: None,
                options,
            })
            .await
        {
            crate::agent::QuestionOutcome::Answered(answer) => Ok(answer),
            crate::agent::QuestionOutcome::ApprovedProposal(answer) => Ok(answer),
            crate::agent::QuestionOutcome::LeftUnanswered => Err("left unanswered".to_string()),
            crate::agent::QuestionOutcome::Cancelled => Err("cancelled".to_string()),
            crate::agent::QuestionOutcome::InputClosed => Err("input closed".to_string()),
            crate::agent::QuestionOutcome::InputUnavailable(error) => Err(error),
        }
    }

    async fn ask_with_context(
        &self,
        context: crate::agent::QuestionRequestContext<'_>,
    ) -> crate::agent::QuestionOutcome {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(TuiQuestionRequest {
                question: context.question.to_string(),
                proposed_answer: context.proposed_answer.map(str::to_string),
                options: context.options.to_vec(),
                reply: reply_tx,
            })
            .is_err()
        {
            return crate::agent::QuestionOutcome::InputUnavailable(
                "TUI question channel closed".to_string(),
            );
        }
        reply_rx
            .await
            .unwrap_or(crate::agent::QuestionOutcome::InputUnavailable(
                "TUI question response was dropped".to_string(),
            ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuestionFocus {
    Editor,
    Suggestions,
    Actions,
}

pub(crate) struct PendingQuestion {
    pub(crate) request: TuiQuestionRequest,
    pub(crate) recovery: Option<RecoveredQuestionTarget>,
    pub(crate) response: String,
    pub(crate) selected_option: Option<usize>,
    pub(crate) selected_decision: usize,
    pub(crate) focus: QuestionFocus,
    pub(crate) error: Option<String>,
}

pub(crate) struct RecoveredQuestionTarget {
    pub(crate) store: SessionStore,
    pub(crate) session_id: String,
    pub(crate) invocation_id: String,
}

pub(crate) const MAX_COMPOSER_BYTES: usize = 16 * 1024;
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PasteInsertion {
    pub(crate) inserted_bytes: usize,
    pub(crate) truncated: bool,
    pub(crate) controls_omitted: bool,
}

impl PasteInsertion {
    pub(crate) fn visible_status(self) -> Option<String> {
        match (self.truncated, self.controls_omitted) {
            (true, true) => Some(format!(
                "[composer] paste truncated at {MAX_COMPOSER_BYTES} bytes; control characters omitted"
            )),
            (true, false) => Some(format!(
                "[composer] paste truncated at {MAX_COMPOSER_BYTES} bytes"
            )),
            (false, true) => {
                Some("[composer] unsafe paste control characters omitted".to_string())
            }
            (false, false) => None,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Composer {
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) history: DraftHistory,
    pub(crate) history_index: Option<usize>,
    pub(crate) stash: Option<String>,
}

impl Composer {
    #[cfg(test)]
    pub(crate) fn from_text(input: impl Into<String>) -> Self {
        let input = input.into();
        Self {
            cursor: input.len(),
            input,
            history: DraftHistory::default(),
            history_index: None,
            stash: None,
        }
    }

    pub(crate) fn set_text(&mut self, input: String) {
        self.cursor = input.len();
        self.input = input;
    }

    pub(crate) fn remember_submission(&mut self, submitted: &str) {
        if submitted.trim().is_empty() {
            return;
        }
        self.history.remember_submission(submitted);
        self.history_index = None;
        self.stash = None;
    }

    pub(crate) fn recall_older(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.stash = Some(self.input.clone());
                self.history.entries().len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_index = Some(next);
        if let Some(entry) = self.history.entry(next) {
            self.set_text(entry.to_string());
        }
    }

    pub(crate) fn recall_newer(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.history.entries().len() {
            self.history_index = None;
            let restored = self.stash.take().unwrap_or_default();
            self.set_text(restored);
            return;
        }
        self.history_index = Some(index + 1);
        if let Some(entry) = self.history.entry(index + 1) {
            self.set_text(entry.to_string());
        }
    }

    pub(crate) fn select_history_entry(&mut self, index: usize) -> bool {
        let Some(entry) = self.history.entry(index).map(str::to_string) else {
            return false;
        };
        if self.history_index.is_none() {
            self.stash = Some(self.input.clone());
        }
        self.history_index = Some(index);
        self.set_text(entry);
        true
    }

    pub(crate) fn clamp_cursor(&mut self) {
        if self.cursor > self.input.len() {
            self.cursor = self.input.len();
        }
        while self.cursor > 0 && !self.input.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    pub(crate) fn move_left(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.clamp_cursor();
    }

    pub(crate) fn move_right(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor += 1;
        while self.cursor < self.input.len() && !self.input.is_char_boundary(self.cursor) {
            self.cursor += 1;
        }
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        self.clamp_cursor();
        if self.input.len().saturating_add(text.len()) > MAX_COMPOSER_BYTES {
            return;
        }
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub(crate) fn insert_paste(&mut self, pasted: &str) -> PasteInsertion {
        self.clamp_cursor();
        let available = MAX_COMPOSER_BYTES.saturating_sub(self.input.len());
        let mut insertion = String::with_capacity(available.min(pasted.len()));
        let mut outcome = PasteInsertion::default();
        let mut characters = pasted.chars().peekable();
        let mut capacity_exhausted = false;

        while let Some(character) = characters.next() {
            let mut encoded = [0u8; 4];
            let normalized = match character {
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    "\n"
                }
                '\n' => "\n",
                '\t' => "    ",
                character if character.is_control() => {
                    outcome.controls_omitted = true;
                    continue;
                }
                character => character.encode_utf8(&mut encoded),
            };
            if capacity_exhausted || insertion.len().saturating_add(normalized.len()) > available {
                outcome.truncated = true;
                capacity_exhausted = true;
                continue;
            }
            insertion.push_str(normalized);
        }

        outcome.inserted_bytes = insertion.len();
        self.input.insert_str(self.cursor, &insertion);
        self.cursor += insertion.len();
        outcome
    }

    pub(crate) fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor;
        self.move_left();
        self.input.replace_range(self.cursor..end, "");
    }

    pub(crate) fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        let mut end = self.cursor + 1;
        while end < self.input.len() && !self.input.is_char_boundary(end) {
            end += 1;
        }
        self.input.replace_range(self.cursor..end, "");
    }
}
