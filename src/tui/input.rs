//! TUI internals split for T043 module size.

use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    Pending,
    Submit(String),
}

pub(crate) fn composer_action_for_key(
    composer: &mut Composer,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ComposerAction {
    match code {
        KeyCode::Left => {
            composer.move_left();
            ComposerAction::Pending
        }
        KeyCode::Right => {
            composer.move_right();
            ComposerAction::Pending
        }
        KeyCode::Home => {
            composer.cursor = 0;
            ComposerAction::Pending
        }
        KeyCode::End => {
            composer.cursor = composer.input.len();
            ComposerAction::Pending
        }
        KeyCode::Up => {
            composer.recall_older();
            ComposerAction::Pending
        }
        KeyCode::Down => {
            composer.recall_newer();
            ComposerAction::Pending
        }
        KeyCode::Char('j') | KeyCode::Char('J') if modifiers.contains(KeyModifiers::CONTROL) => {
            composer.insert_str("\n");
            ComposerAction::Pending
        }
        KeyCode::Enter
            if modifiers.contains(KeyModifiers::SHIFT) || modifiers.contains(KeyModifiers::ALT) =>
        {
            composer.insert_str("\n");
            ComposerAction::Pending
        }
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
            composer.insert_str(&character.to_string());
            ComposerAction::Pending
        }
        KeyCode::Backspace => {
            composer.backspace();
            ComposerAction::Pending
        }
        KeyCode::Delete => {
            composer.delete();
            ComposerAction::Pending
        }
        KeyCode::Enter => {
            if composer.input.trim().is_empty() {
                ComposerAction::Pending
            } else {
                let submitted = std::mem::take(&mut composer.input);
                composer.cursor = 0;
                composer.remember_submission(&submitted);
                ComposerAction::Submit(submitted)
            }
        }
        KeyCode::Esc => ComposerAction::Pending,
        _ => ComposerAction::Pending,
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CompletionMenu {
    pub(crate) suggestions: Vec<InteractiveCompletion>,
    pub(crate) selected: usize,
    pub(crate) dismissed_for_input: Option<String>,
}

impl CompletionMenu {
    #[cfg(test)]
    pub(crate) fn sync(&mut self, input: &str) {
        self.sync_for(input, None);
    }

    pub(crate) fn sync_for(&mut self, input: &str, project_root: Option<&Path>) {
        if self.dismissed_for_input.as_deref() == Some(input) {
            self.suggestions.clear();
            self.selected = 0;
            return;
        }
        self.dismissed_for_input = None;
        self.suggestions = if input.starts_with('/') {
            interactive_completions(input)
        } else {
            project_root
                .map(|root| path_completions(root, input))
                .unwrap_or_default()
        };
        if self.selected >= self.suggestions.len() {
            self.selected = 0;
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        !self.suggestions.is_empty()
    }

    pub(crate) fn handle_key(
        &mut self,
        composer: &mut Composer,
        code: KeyCode,
    ) -> CompletionKeyResult {
        if !self.is_open() {
            return CompletionKeyResult::Ignored;
        }
        match code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                CompletionKeyResult::Consumed
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.suggestions.len() - 1);
                CompletionKeyResult::Consumed
            }
            KeyCode::Tab => {
                if let Some(completion) = self.suggestions.get(self.selected) {
                    composer.set_text(completion.insertion.clone());
                }
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
                CompletionKeyResult::Consumed
            }
            KeyCode::Enter => {
                if let Some(completion) = self.suggestions.get(self.selected) {
                    composer.set_text(completion.insertion.clone());
                }
                let runnable = !composer.input.ends_with(' ');
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
                if runnable && !composer.input.trim().is_empty() {
                    CompletionKeyResult::Submit
                } else {
                    CompletionKeyResult::Consumed
                }
            }
            KeyCode::Esc => {
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
                CompletionKeyResult::Consumed
            }
            _ => CompletionKeyResult::Ignored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingHistorySearch {
    pub(crate) query: String,
    pub(crate) search: DraftHistorySearch,
    pub(crate) selected: usize,
    pub(crate) error: Option<String>,
}

impl PendingHistorySearch {
    pub(crate) fn new(history: &DraftHistory, query: Option<String>) -> Self {
        let search = history.search(query.as_deref().unwrap_or_default());
        let error = history_search_notice(&search, history.is_empty());
        Self {
            query: search.query.clone(),
            search,
            selected: 0,
            error,
        }
    }

    pub(crate) fn refresh(&mut self, history: &DraftHistory) {
        self.search = history.search(&self.query);
        self.query = self.search.query.clone();
        self.selected = self
            .selected
            .min(self.search.matches.len().saturating_sub(1));
        self.error = history_search_notice(&self.search, history.is_empty());
    }

    pub(crate) fn insert(&mut self, character: char, history: &DraftHistory) {
        if character.is_control() {
            self.error = Some("[history error] control character ignored".to_string());
            return;
        }
        if self.query.len().saturating_add(character.len_utf8()) > MAX_DRAFT_HISTORY_QUERY_BYTES {
            self.error = Some(format!(
                "[history error] query is limited to {MAX_DRAFT_HISTORY_QUERY_BYTES} bytes"
            ));
            return;
        }
        self.query.push(character);
        self.selected = 0;
        self.refresh(history);
    }

    pub(crate) fn backspace(&mut self, history: &DraftHistory) {
        self.query.pop();
        self.selected = 0;
        self.refresh(history);
    }
}

pub(crate) fn history_search_notice(
    search: &DraftHistorySearch,
    history_empty: bool,
) -> Option<String> {
    if search.query_truncated {
        Some(format!(
            "[history error] query truncated at {MAX_DRAFT_HISTORY_QUERY_BYTES} bytes"
        ))
    } else if search.controls_omitted {
        Some("[history error] control characters omitted from query".to_string())
    } else if history_empty {
        Some("[history] no submitted drafts are available".to_string())
    } else if search.matches.is_empty() {
        Some("[history] no matching submitted drafts".to_string())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistorySearchAction {
    Pending,
    Close,
    Select(usize),
}

pub(crate) fn history_search_action_for_key(
    search: &mut PendingHistorySearch,
    history: &DraftHistory,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> HistorySearchAction {
    match code {
        KeyCode::Up => {
            search.selected = search.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            search.selected = search
                .selected
                .saturating_add(1)
                .min(search.search.matches.len().saturating_sub(1));
        }
        KeyCode::Backspace => search.backspace(history),
        KeyCode::Char(character)
            if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            search.insert(character, history);
        }
        KeyCode::Enter => {
            if let Some(result) = search.search.matches.get(search.selected) {
                return HistorySearchAction::Select(result.entry_index);
            }
            search.error = Some("[history error] select requires a matching draft".to_string());
        }
        KeyCode::Esc => return HistorySearchAction::Close,
        _ => {}
    }
    HistorySearchAction::Pending
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionLayer {
    Approval,
    Question,
    Model,
    SessionConfirmation,
    SessionSwitcher,
    HistorySearch,
    Completion,
    Composer,
    RecoverableError,
}

pub(crate) fn tui_interaction_state(
    approval: bool,
    question: bool,
    model: bool,
    switcher: Option<&SessionSwitcher>,
    history_search: bool,
    completion: bool,
    run: InteractionRunState,
) -> InteractionState {
    InteractionState {
        approval_pending: approval,
        question_pending: question,
        destructive_confirmation_pending: switcher.is_some_and(|switcher| switcher.confirming),
        selector_or_detail: (model
            || history_search
            || switcher.is_some_and(|switcher| !switcher.confirming))
        .then_some(SelectorDetailKind::Selector),
        completion_pending: completion,
        run,
    }
}

pub(crate) fn tui_run_state(
    worker_active: bool,
    live_state: Option<&str>,
    reconciled_terminal: Option<InteractionTerminalOutcome>,
) -> InteractionRunState {
    if worker_active {
        match live_state {
            Some("planning") => InteractionRunState::Planning,
            Some("reconciliation") => InteractionRunState::Reconciling,
            _ => InteractionRunState::Running,
        }
    } else {
        match reconciled_terminal {
            Some(InteractionTerminalOutcome::Completed) => InteractionRunState::Completed,
            Some(InteractionTerminalOutcome::Cancelled) => InteractionRunState::Cancelled,
            Some(
                InteractionTerminalOutcome::Failed | InteractionTerminalOutcome::WaitingForInput,
            ) => InteractionRunState::Failed,
            None => InteractionRunState::Idle,
        }
    }
}

pub(crate) fn active_interaction_layer(
    approval: bool,
    question: bool,
    model: bool,
    switcher: Option<&SessionSwitcher>,
    history_search: bool,
    completion: bool,
) -> InteractionLayer {
    let state = tui_interaction_state(
        approval,
        question,
        model,
        switcher,
        history_search,
        completion,
        InteractionRunState::Idle,
    );
    let consumer = match reduce_interaction(&state, InteractionInput::UserAction) {
        InteractionReduction::Consumed(consumer) => consumer,
        _ => return InteractionLayer::RecoverableError,
    };
    match consumer {
        InteractionConsumer::Approval => InteractionLayer::Approval,
        InteractionConsumer::Question => InteractionLayer::Question,
        InteractionConsumer::DestructiveConfirmation => InteractionLayer::SessionConfirmation,
        InteractionConsumer::Selector if history_search => InteractionLayer::HistorySearch,
        InteractionConsumer::Selector if model => InteractionLayer::Model,
        InteractionConsumer::Selector => InteractionLayer::SessionSwitcher,
        InteractionConsumer::Completion => InteractionLayer::Completion,
        InteractionConsumer::Composer => InteractionLayer::Composer,
        InteractionConsumer::Detail | InteractionConsumer::Timeline => {
            InteractionLayer::RecoverableError
        }
    }
}

pub(crate) struct PendingModelSelection {
    pub(crate) selection: ModelSelection,
    pub(crate) response: String,
    pub(crate) selected_option: usize,
}

impl PendingModelSelection {
    pub(crate) fn new(selection: ModelSelection) -> Self {
        let selected_option = selection
            .available
            .iter()
            .position(|model| model == &selection.current)
            .unwrap_or(0);
        Self {
            selection,
            response: String::new(),
            selected_option,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ModelAction {
    Pending,
    Submit(String),
    Cancel,
}

pub(crate) fn model_action_for_key(
    model: &mut PendingModelSelection,
    code: KeyCode,
) -> ModelAction {
    match code {
        KeyCode::Char(character)
            if model.response.len().saturating_add(character.len_utf8()) <= MAX_COMPOSER_BYTES =>
        {
            model.response.push(character);
            ModelAction::Pending
        }
        KeyCode::Backspace => {
            model.response.pop();
            ModelAction::Pending
        }
        KeyCode::Up => {
            model.selected_option = model.selected_option.saturating_sub(1);
            ModelAction::Pending
        }
        KeyCode::Down | KeyCode::Tab => {
            if !model.selection.available.is_empty() {
                model.selected_option = (model.selected_option + 1)
                    .min(model.selection.available.len().saturating_sub(1));
            }
            ModelAction::Pending
        }
        KeyCode::Enter => {
            let response = model.response.trim();
            if let Ok(index) = response.parse::<usize>() {
                if index > 0 {
                    if let Some(selected) = model.selection.available.get(index - 1) {
                        return ModelAction::Submit(selected.clone());
                    }
                }
            }
            if !response.is_empty() {
                ModelAction::Submit(response.to_string())
            } else if let Some(selected) = model.selection.available.get(model.selected_option) {
                ModelAction::Submit(selected.clone())
            } else {
                ModelAction::Pending
            }
        }
        KeyCode::Esc => ModelAction::Cancel,
        _ => ModelAction::Pending,
    }
}

impl PendingQuestion {
    pub(crate) fn new(request: TuiQuestionRequest) -> Self {
        let has_proposal = request.proposed_answer.is_some();
        Self {
            request,
            recovery: None,
            response: String::new(),
            selected_option: None,
            selected_decision: 1,
            focus: if has_proposal {
                QuestionFocus::Actions
            } else {
                QuestionFocus::Editor
            },
            error: None,
        }
    }

    pub(crate) fn recovered(request: TuiQuestionRequest, target: RecoveredQuestionTarget) -> Self {
        let mut pending = Self::new(request);
        pending.recovery = Some(target);
        pending
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QuestionAction {
    Pending,
    Submit(String),
    ApproveProposal(String),
    LeaveUnanswered,
    Error(String),
}

pub(crate) fn submit_question_answer(question: &PendingQuestion, answer: &str) -> QuestionAction {
    let state = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(
        &state,
        InteractionInput::QuestionAnswer {
            answer,
            options: &question.request.options,
            selected_option: question.selected_option,
        },
    ) {
        InteractionReduction::QuestionAnswered(answer) => QuestionAction::Submit(answer),
        InteractionReduction::QuestionLeftUnanswered => QuestionAction::LeaveUnanswered,
        InteractionReduction::Error { message, .. } => QuestionAction::Error(message),
        InteractionReduction::ModalCommand(_) => QuestionAction::Error(
            crate::interactive::modal_command_unsupported_message().to_string(),
        ),
        _ => QuestionAction::Error("question input was rejected by the shared reducer".to_string()),
    }
}

pub(crate) fn question_action_for_key(
    question: &mut PendingQuestion,
    code: KeyCode,
) -> QuestionAction {
    match code {
        KeyCode::Tab => {
            let has_suggestions =
                question.request.proposed_answer.is_none() && !question.request.options.is_empty();
            question.focus = match question.focus {
                QuestionFocus::Editor if has_suggestions => QuestionFocus::Suggestions,
                QuestionFocus::Editor | QuestionFocus::Suggestions => QuestionFocus::Actions,
                QuestionFocus::Actions => QuestionFocus::Editor,
            };
            QuestionAction::Pending
        }
        KeyCode::BackTab => {
            let has_suggestions =
                question.request.proposed_answer.is_none() && !question.request.options.is_empty();
            question.focus = match question.focus {
                QuestionFocus::Editor => QuestionFocus::Actions,
                QuestionFocus::Suggestions => QuestionFocus::Editor,
                QuestionFocus::Actions if !has_suggestions => QuestionFocus::Editor,
                QuestionFocus::Actions => QuestionFocus::Suggestions,
            };
            QuestionAction::Pending
        }
        KeyCode::Char(character)
            if question.focus == QuestionFocus::Editor
                && question.response.len().saturating_add(character.len_utf8())
                    <= MAX_COMPOSER_BYTES =>
        {
            question.response.push(character);
            question.error = None;
            QuestionAction::Pending
        }
        KeyCode::Backspace if question.focus == QuestionFocus::Editor => {
            question.response.pop();
            question.error = None;
            QuestionAction::Pending
        }
        KeyCode::Up if question.focus == QuestionFocus::Suggestions => {
            if let Some(selected) = question.selected_option {
                question.selected_option = selected.checked_sub(1);
            }
            QuestionAction::Pending
        }
        KeyCode::Up
            if question.focus == QuestionFocus::Actions
                && question.request.proposed_answer.is_some() =>
        {
            question.selected_decision = question.selected_decision.saturating_sub(1);
            QuestionAction::Pending
        }
        KeyCode::Down
            if question.focus == QuestionFocus::Actions
                && question.request.proposed_answer.is_some() =>
        {
            question.selected_decision = question.selected_decision.saturating_add(1).min(2);
            QuestionAction::Pending
        }
        KeyCode::Char(digit @ '1'..='3')
            if question.focus == QuestionFocus::Actions
                && question.request.proposed_answer.is_some() =>
        {
            question.selected_decision = usize::from(digit as u8 - b'1');
            QuestionAction::Pending
        }
        KeyCode::Down if question.focus == QuestionFocus::Suggestions => {
            let last = question.request.options.len().saturating_sub(1);
            question.selected_option = Some(match question.selected_option {
                Some(selected) => selected.saturating_add(1).min(last),
                None => 0,
            });
            QuestionAction::Pending
        }
        KeyCode::Enter => match question.focus {
            QuestionFocus::Actions => {
                if let Some(proposal) = question.request.proposed_answer.as_deref() {
                    match question.selected_decision {
                        0 => QuestionAction::ApproveProposal(proposal.to_string()),
                        1 => QuestionAction::LeaveUnanswered,
                        _ => {
                            question.focus = QuestionFocus::Editor;
                            QuestionAction::Pending
                        }
                    }
                } else {
                    QuestionAction::LeaveUnanswered
                }
            }
            QuestionFocus::Suggestions => {
                if let Some(index) = question.selected_option {
                    if let Some(option) = question.request.options.get(index) {
                        return QuestionAction::Submit(option.clone());
                    }
                }
                QuestionAction::Error("select an option or type a custom answer".to_string())
            }
            QuestionFocus::Editor => submit_question_answer(question, &question.response),
        },
        KeyCode::Esc => QuestionAction::LeaveUnanswered,
        _ => QuestionAction::Pending,
    }
}

pub(crate) fn paste_question_answer(question: &mut PendingQuestion, pasted: &str) {
    if question.focus != QuestionFocus::Editor {
        return;
    }
    let input = std::mem::take(&mut question.response);
    let mut editor = Composer {
        cursor: input.len(),
        input,
        ..Composer::default()
    };
    let outcome = editor.insert_paste(pasted);
    question.response = editor.input;
    question.error = outcome.visible_status();
}

pub(crate) fn open_prompt_command_overlay(
    code: KeyCode,
    prompt_pending: bool,
    command_overlay: &mut Option<String>,
) -> bool {
    if matches!(code, KeyCode::F(2)) && prompt_pending && command_overlay.is_none() {
        *command_overlay = Some(String::new());
        true
    } else {
        false
    }
}

pub(crate) fn handle_question_key(question: &mut Option<PendingQuestion>, code: KeyCode) -> bool {
    let Some(pending) = question.as_mut() else {
        return false;
    };
    let action = question_action_for_key(pending, code);
    match action {
        QuestionAction::Pending => false,
        QuestionAction::ApproveProposal(response) => {
            if let Some(target) = question
                .as_ref()
                .and_then(|pending| pending.recovery.as_ref())
            {
                match crate::interactive::persist_recovered_proposed_answer(
                    &target.store,
                    &target.session_id,
                    &target.invocation_id,
                    &response,
                ) {
                    Ok(_) => {
                        question.take();
                        return true;
                    }
                    Err(message) => {
                        if let Some(pending) = question.as_mut() {
                            pending.error = Some(message);
                        }
                        return false;
                    }
                }
            }
            if let Some(pending) = question.take() {
                let _ = pending
                    .request
                    .reply
                    .send(crate::agent::QuestionOutcome::ApprovedProposal(response));
            }
            true
        }
        QuestionAction::Submit(response) => {
            if let Some(target) = question
                .as_ref()
                .and_then(|pending| pending.recovery.as_ref())
            {
                match crate::interactive::persist_recovered_question_answer(
                    &target.store,
                    &target.session_id,
                    &target.invocation_id,
                    &response,
                ) {
                    Ok(_) => {
                        question.take();
                        return true;
                    }
                    Err(message) => {
                        if let Some(pending) = question.as_mut() {
                            pending.error = Some(message);
                        }
                        return false;
                    }
                }
            }
            if let Some(pending) = question.take() {
                let _ = pending
                    .request
                    .reply
                    .send(crate::agent::QuestionOutcome::Answered(response));
            }
            true
        }
        QuestionAction::LeaveUnanswered => {
            if let Some(pending) = question.take() {
                let _ = pending
                    .request
                    .reply
                    .send(crate::agent::QuestionOutcome::LeftUnanswered);
            }
            true
        }
        QuestionAction::Error(message) => {
            if let Some(pending) = question.as_mut() {
                pending.error = Some(message);
            }
            false
        }
    }
}

pub(crate) fn approval_decision_from_answer(answer: &str) -> Result<ApprovalDecision, String> {
    let state = InteractionState {
        approval_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(&state, InteractionInput::ApprovalAnswer(answer)) {
        InteractionReduction::ApprovalDecision(InteractionDecision::Accept) => {
            Ok(ApprovalDecision::granted_user())
        }
        InteractionReduction::ApprovalDecision(InteractionDecision::Reject) => {
            Ok(ApprovalDecision::denied())
        }
        InteractionReduction::Error { message, .. } => Err(message),
        _ => Err("approval input was rejected".to_string()),
    }
}

pub(crate) fn card_input_for_key(code: KeyCode) -> crate::interaction_card::CardInput {
    match code {
        KeyCode::Esc => crate::interaction_card::CardInput::Esc,
        KeyCode::Up => crate::interaction_card::CardInput::Up,
        KeyCode::Down => crate::interaction_card::CardInput::Down,
        KeyCode::Enter => crate::interaction_card::CardInput::Enter,
        KeyCode::Backspace => crate::interaction_card::CardInput::Backspace,
        KeyCode::Char(character) => crate::interaction_card::CardInput::Char(character),
        _ => crate::interaction_card::CardInput::Other,
    }
}

pub(crate) fn command_rows(req: &TuiApprovalRequest) -> Vec<crate::interaction_card::CardRow> {
    let command = req.context.shown_command.clone().unwrap_or_else(|| {
        req.call
            .arguments
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    });
    let remember = req.context.remember_exact.as_deref();
    crate::interaction_card::command_approval_card(
        "local",
        &req.context.reason,
        &command,
        &req.context.command_extras,
        remember,
        req.selected_option,
        true,
    )
    .rows
}

pub(crate) fn finish_command_reason(approval: &mut Option<TuiApprovalRequest>) {
    let Some(pending) = approval.as_mut() else {
        return;
    };
    let draft = pending.reason_draft.clone().unwrap_or_default();
    let reason = draft.trim();
    if reason.len() > 240 {
        pending.error = Some("Input error: the reason is too long".to_string());
        return;
    }
    let decision = if reason.is_empty() {
        ApprovalDecision::denied()
    } else {
        ApprovalDecision::denied_with_reason(reason.to_string())
    };
    if let Some(request) = approval.take() {
        let _ = request.reply.send(decision);
    }
}

pub(crate) fn handle_command_approval_key(
    approval: &mut Option<TuiApprovalRequest>,
    code: KeyCode,
) -> bool {
    let Some(pending) = approval.as_ref() else {
        return false;
    };
    let rows = command_rows(pending);
    let editing = pending.reason_draft.is_some();
    let selected = pending.selected_option;
    let action =
        crate::interaction_card::card_key(&rows, selected, card_input_for_key(code), editing);
    match action {
        crate::interaction_card::CardKey::Ignored => true,
        crate::interaction_card::CardKey::Moved(index) => {
            if let Some(pending) = approval.as_mut() {
                pending.selected_option = index;
                if editing {
                    pending.reason_draft = None;
                }
                pending.error = None;
            }
            true
        }
        crate::interaction_card::CardKey::Cancel => {
            if let Some(request) = approval.take() {
                let _ = request.reply.send(ApprovalDecision::denied());
            }
            true
        }
        crate::interaction_card::CardKey::Edit(character) => {
            if let Some(pending) = approval.as_mut() {
                if let Some(draft) = pending.reason_draft.as_mut() {
                    if draft.len().saturating_add(character.len_utf8()) <= 240 {
                        draft.push(character);
                        pending.error = None;
                    } else {
                        pending.error = Some("Input error: the reason is too long".to_string());
                    }
                }
            }
            true
        }
        crate::interaction_card::CardKey::Backspace => {
            if let Some(pending) = approval.as_mut() {
                if let Some(draft) = pending.reason_draft.as_mut() {
                    draft.pop();
                    pending.error = None;
                }
            }
            true
        }
        crate::interaction_card::CardKey::Activate(index) => {
            if editing {
                finish_command_reason(approval);
                return true;
            }
            let role = rows.get(index).map(|row| row.role);
            match role {
                Some(crate::interaction_card::CommandRowRole::Yes) => {
                    if let Some(request) = approval.take() {
                        let _ = request.reply.send(ApprovalDecision::granted_user());
                    }
                }
                Some(crate::interaction_card::CommandRowRole::Remember) => {
                    let command = approval
                        .as_ref()
                        .and_then(|pending| pending.context.remember_exact.clone())
                        .unwrap_or_default();
                    if let Some(request) = approval.take() {
                        let _ = request
                            .reply
                            .send(ApprovalDecision::granted_remembered(command));
                    }
                }
                Some(crate::interaction_card::CommandRowRole::No) => {
                    if let Some(pending) = approval.as_mut() {
                        pending.reason_draft = Some(String::new());
                        pending.error = None;
                    }
                }
                None => {}
            }
            true
        }
    }
}

pub(crate) fn handle_approval_key(
    approval: &mut Option<TuiApprovalRequest>,
    code: KeyCode,
) -> bool {
    let Some(pending) = approval.as_mut() else {
        return false;
    };
    if pending.call.tool_name == "run_terminal" {
        return handle_command_approval_key(approval, code);
    }
    if pending.details_open {
        match code {
            KeyCode::Esc => pending.details_open = false,
            KeyCode::Up => pending.detail_offset = pending.detail_offset.saturating_sub(1),
            KeyCode::Down => pending.detail_offset = pending.detail_offset.saturating_add(1),
            KeyCode::PageUp => pending.detail_offset = pending.detail_offset.saturating_sub(5),
            KeyCode::PageDown => pending.detail_offset = pending.detail_offset.saturating_add(5),
            _ => {}
        }
        return true;
    }
    match code {
        KeyCode::Esc => {
            if let Some(request) = approval.take() {
                let _ = request.reply.send(ApprovalDecision::denied());
            }
            return true;
        }
        KeyCode::Up => {
            pending.selected_option = pending.selected_option.saturating_sub(1);
            pending.typed.clear();
            pending.error = None;
            return true;
        }
        KeyCode::Down | KeyCode::Tab => {
            pending.selected_option = (pending.selected_option + 1).min(2);
            pending.typed.clear();
            pending.error = None;
            return true;
        }
        KeyCode::Char(character) if !character.is_control() => {
            pending.typed.push(character);
            pending.error = None;
            return true;
        }
        KeyCode::Backspace => {
            pending.typed.pop();
            pending.error = None;
            return true;
        }
        KeyCode::Enter => {
            let typed = pending.typed.trim().to_string();
            if typed.is_empty() && pending.selected_option == 2 {
                pending.details_open = true;
                pending.detail_offset = 0;
                return true;
            }
            let answer = if typed.is_empty() {
                if pending.selected_option == 0 {
                    "y"
                } else {
                    "n"
                }
            } else {
                typed.as_str()
            };
            match approval_decision_from_answer(answer) {
                Ok(decision) => {
                    if let Some(request) = approval.take() {
                        let _ = request.reply.send(decision);
                    }
                }
                Err(message) => {
                    pending.typed.clear();
                    pending.error = Some(message);
                }
            }
            return true;
        }
        _ => {}
    }
    false
}

pub(crate) fn handle_pending_interaction_key(
    approval: &mut Option<TuiApprovalRequest>,
    question: &mut Option<PendingQuestion>,
    code: KeyCode,
) -> bool {
    if approval.is_some() {
        handle_approval_key(approval, code);
        return true;
    }
    if question.is_some() {
        handle_question_key(question, code);
        return true;
    }
    false
}
