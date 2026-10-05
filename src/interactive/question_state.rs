//! Presentation-neutral keyboard state for a question call.

use super::{
    parse_question_editor_input, FormQuestion, QuestionAnswer, QuestionAnswerSource,
    QuestionEditorInput, QuestionForm, QuestionFormOutcome,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionEditorKind {
    Answer,
    Discussion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionEditor {
    pub kind: QuestionEditorKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionFormEvent {
    PreviousRow,
    NextRow,
    SelectRow(usize),
    PreviousTab,
    NextTab,
    Choose,
    Type(char),
    Backspace,
    ReplaceEditor(String),
    Interrupt,
}

#[derive(Debug, Clone)]
pub struct QuestionFormState {
    pub form: QuestionForm,
    pub drafts: Vec<Option<QuestionAnswer>>,
    pub tab: usize,
    pub selected_row: usize,
    pub editor: Option<QuestionEditor>,
    pub error: Option<String>,
    editor_drafts: Vec<Option<QuestionEditor>>,
}

impl QuestionFormState {
    pub fn new(form: QuestionForm, initial: &[Option<QuestionAnswer>]) -> Self {
        let drafts = (0..form.questions.len())
            .map(|index| initial.get(index).cloned().flatten())
            .collect();
        let editor_drafts = vec![None; form.questions.len()];
        Self { form, drafts, tab: 0, selected_row: 0, editor: None, error: None, editor_drafts }
    }

    pub fn current_question(&self) -> Option<&FormQuestion> {
        self.form.questions.get(self.tab)
    }

    pub fn is_submit(&self) -> bool {
        self.form.questions.len() > 1 && self.tab == self.form.questions.len()
    }

    pub fn complete(&self) -> bool {
        !self.drafts.is_empty() && self.drafts.iter().all(Option::is_some)
    }

    pub fn row_count(&self) -> usize {
        self.current_question().map_or(1, |question| {
            if question.proposed_answer.is_some() { 4 } else { question.options.len() + 2 }
        })
    }

    pub fn footer(&self) -> &'static str {
        if self.editor.is_some() {
            "Enter submit · Esc interrupt operation"
        } else {
            "Up/Down select · Enter choose · Esc interrupt operation"
        }
    }

    pub fn apply(&mut self, event: QuestionFormEvent) -> Option<QuestionFormOutcome> {
        if event == QuestionFormEvent::Interrupt {
            self.discard_drafts();
            return Some(QuestionFormOutcome::LeftUnanswered);
        }
        if matches!(event, QuestionFormEvent::PreviousTab | QuestionFormEvent::NextTab) {
            self.move_tab(event == QuestionFormEvent::NextTab);
            return None;
        }
        if self.editor.is_some() { return self.edit(event); }
        match event {
            QuestionFormEvent::PreviousRow => {
                self.selected_row = self.selected_row.saturating_sub(1);
            }
            QuestionFormEvent::NextRow => {
                self.selected_row = (self.selected_row + 1).min(self.row_count() - 1);
            }
            QuestionFormEvent::SelectRow(index) if index < self.row_count() => {
                self.selected_row = index;
            }
            QuestionFormEvent::Choose => return self.choose(),
            QuestionFormEvent::Type(character) if !self.is_submit() => {
                self.open_editor(QuestionEditorKind::Answer);
                return self.edit(QuestionFormEvent::Type(character));
            }
            _ => {}
        }
        None
    }

    fn move_tab(&mut self, next: bool) {
        if self.form.questions.len() < 2 { return; }
        let count = self.form.questions.len() + 1;
        let tab = if next { (self.tab + 1) % count } else { (self.tab + count - 1) % count };
        self.focus_tab(tab);
    }

    pub fn focus_tab(&mut self, tab: usize) {
        if let Some(slot) = self.editor_drafts.get_mut(self.tab) { *slot = self.editor.take(); }
        self.tab = tab.min(self.form.questions.len());
        self.selected_row = 0;
        self.editor = self.editor_drafts.get_mut(self.tab).and_then(Option::take);
        self.error = None;
    }

    fn choose(&mut self) -> Option<QuestionFormOutcome> {
        self.error = None;
        if self.is_submit() {
            if !self.complete() {
                self.focus_tab(self.drafts.iter().position(Option::is_none).unwrap_or(0));
                self.error = Some("Answer every question before Submit.".to_string());
                return None;
            }
            return Some(QuestionFormOutcome::Answered(
                self.drafts.iter().filter_map(Clone::clone).collect(),
            ));
        }
        let question = self.current_question()?;
        if let Some(proposal) = &question.proposed_answer {
            return match self.selected_row {
                0 => self.answer(QuestionAnswer { answer: proposal.clone(), source: QuestionAnswerSource::ApprovedProposal }),
                1 => { self.discard_drafts(); Some(QuestionFormOutcome::LeftUnanswered) }
                2 => { self.open_editor(QuestionEditorKind::Answer); None }
                _ => { self.open_editor(QuestionEditorKind::Discussion); None }
            };
        }
        let count = question.options.len();
        if self.selected_row < count {
            return self.answer(QuestionAnswer {
                answer: question.options[self.selected_row].label.clone(),
                source: QuestionAnswerSource::Option,
            });
        }
        self.open_editor(if self.selected_row == count { QuestionEditorKind::Answer } else { QuestionEditorKind::Discussion });
        None
    }

    pub fn open_editor(&mut self, kind: QuestionEditorKind) {
        let text = if kind == QuestionEditorKind::Answer {
            self.drafts.get(self.tab).and_then(Option::as_ref)
                .filter(|answer| answer.source == QuestionAnswerSource::Text)
                .map(|answer| answer.answer.clone()).unwrap_or_default()
        } else { String::new() };
        self.editor = Some(QuestionEditor { kind, text });
    }

    fn edit(&mut self, event: QuestionFormEvent) -> Option<QuestionFormOutcome> {
        let editor = self.editor.as_mut()?;
        match event {
            QuestionFormEvent::Type(character) if editor.text.len() + character.len_utf8() <= 16 * 1024 => {
                editor.text.push(character);
                self.error = None;
            }
            QuestionFormEvent::Backspace => { editor.text.pop(); self.error = None; }
            QuestionFormEvent::ReplaceEditor(text) => { editor.text = text; self.error = None; }
            QuestionFormEvent::Choose => return self.submit_editor(),
            _ => {}
        }
        None
    }

    fn submit_editor(&mut self) -> Option<QuestionFormOutcome> {
        let editor = self.editor.as_ref()?;
        if editor.text.trim_start().starts_with(":command") {
            self.error = Some(super::modal_command_unsupported_message().to_string());
            return None;
        }
        match parse_question_editor_input(&editor.text) {
            QuestionEditorInput::Interrupt => {
                self.discard_drafts();
                Some(QuestionFormOutcome::LeftUnanswered)
            }
            QuestionEditorInput::Retry(message) => { self.error = Some(message); None }
            QuestionEditorInput::Text(text) if editor.kind == QuestionEditorKind::Discussion => {
                self.discard_drafts();
                Some(QuestionFormOutcome::Discussed(text))
            }
            QuestionEditorInput::Text(answer) => self.answer(QuestionAnswer { answer, source: QuestionAnswerSource::Text }),
        }
    }

    fn answer(&mut self, answer: QuestionAnswer) -> Option<QuestionFormOutcome> {
        if self.form.questions.len() == 1 { return Some(QuestionFormOutcome::Answered(vec![answer])); }
        self.drafts[self.tab] = Some(answer);
        self.tab = self.drafts.iter().position(Option::is_none).unwrap_or(self.form.questions.len());
        self.selected_row = 0;
        self.editor = self.editor_drafts.get_mut(self.tab).and_then(Option::take);
        self.error = None;
        None
    }

    fn discard_drafts(&mut self) {
        self.drafts.fill(None);
        self.editor_drafts.fill(None);
        self.editor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::QuestionOption;

    fn state(count: usize) -> QuestionFormState {
        let form = QuestionForm { header: None, questions: (0..count).map(|index| FormQuestion {
            title: Some(format!("Question {index}")), question: format!("Choose {index}?"), proposed_answer: None,
            options: vec![QuestionOption { label: "First".to_string(), description: Some("Description".to_string()) }, QuestionOption { label: "Second".to_string(), description: None }],
        }).collect() };
        QuestionFormState::new(form, &[])
    }

    #[test]
    fn first_row_and_digits_require_enter_and_store_label() {
        let mut state = state(1);
        assert_eq!(state.selected_row, 0);
        assert!(state.apply(QuestionFormEvent::SelectRow(1)).is_none());
        assert_eq!(state.apply(QuestionFormEvent::Choose), Some(QuestionFormOutcome::Answered(vec![QuestionAnswer { answer: "Second".to_string(), source: QuestionAnswerSource::Option }])));
    }

    #[test]
    fn set_drafts_survive_wrapping_tabs_and_only_submit_emits_answers() {
        let mut state = state(2);
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert_eq!(state.tab, 1);
        state.apply(QuestionFormEvent::PreviousTab);
        assert_eq!(state.drafts[0].as_ref().unwrap().answer, "First");
        state.apply(QuestionFormEvent::PreviousTab);
        assert!(state.is_submit());
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert_eq!(state.tab, 1);
        assert!(state.error.is_some());
        state.apply(QuestionFormEvent::SelectRow(1));
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert!(state.is_submit());
        let Some(QuestionFormOutcome::Answered(answers)) = state.apply(QuestionFormEvent::Choose) else { panic!("set must submit") };
        assert_eq!(answers.iter().map(|answer| answer.answer.as_str()).collect::<Vec<_>>(), ["First", "Second"]);
    }

    #[test]
    fn text_editor_keeps_digits_and_y_literal_and_retries_empty_input() {
        let mut state = state(1);
        state.apply(QuestionFormEvent::SelectRow(2));
        state.apply(QuestionFormEvent::Choose);
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert!(state.error.is_some());
        for character in "text: 1Y".chars() { state.apply(QuestionFormEvent::Type(character)); }
        assert_eq!(state.apply(QuestionFormEvent::Choose), Some(QuestionFormOutcome::Answered(vec![QuestionAnswer { answer: "1Y".to_string(), source: QuestionAnswerSource::Text }])));
    }

    #[test]
    fn discussion_and_interrupt_discard_unsubmitted_set_drafts() {
        let mut state = state(2);
        state.apply(QuestionFormEvent::Choose);
        state.apply(QuestionFormEvent::SelectRow(3));
        state.apply(QuestionFormEvent::Choose);
        state.apply(QuestionFormEvent::ReplaceEditor("Need details".to_string()));
        assert_eq!(state.apply(QuestionFormEvent::Choose), Some(QuestionFormOutcome::Discussed("Need details".to_string())));
        assert!(state.drafts.iter().all(Option::is_none));
        state.apply(QuestionFormEvent::Choose);
        assert_eq!(state.apply(QuestionFormEvent::Interrupt), Some(QuestionFormOutcome::LeftUnanswered));
        assert!(state.drafts.iter().all(Option::is_none));
    }

    #[test]
    fn proposal_rows_approve_exactly_or_interrupt_or_edit_or_discuss() {
        let mut state = state(1);
        state.form.questions[0].proposed_answer = Some("Exact proposal".to_string());
        assert_eq!(state.row_count(), 4);
        assert_eq!(state.apply(QuestionFormEvent::Choose), Some(QuestionFormOutcome::Answered(vec![QuestionAnswer { answer: "Exact proposal".to_string(), source: QuestionAnswerSource::ApprovedProposal }])));
        state.apply(QuestionFormEvent::SelectRow(2));
        state.apply(QuestionFormEvent::Choose);
        assert_eq!(state.editor.as_ref().unwrap().kind, QuestionEditorKind::Answer);
        assert_eq!(state.apply(QuestionFormEvent::Interrupt), Some(QuestionFormOutcome::LeftUnanswered));
        state.apply(QuestionFormEvent::SelectRow(3));
        state.apply(QuestionFormEvent::Choose);
        assert_eq!(state.editor.as_ref().unwrap().kind, QuestionEditorKind::Discussion);
    }

    #[test]
    fn reused_drafts_are_checked_and_can_be_replaced() {
        let mut state = state(2);
        state.drafts[0] = Some(QuestionAnswer { answer: "First".to_string(), source: QuestionAnswerSource::Option });
        state.apply(QuestionFormEvent::SelectRow(1));
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert_eq!(state.drafts[0].as_ref().unwrap().answer, "Second");
        assert_eq!(state.tab, 1);
    }
    #[test]
    fn unsaved_editor_text_survives_tab_navigation_and_y_never_approves() {
        let mut state = state(2);
        state.form.questions[0].proposed_answer = Some("Proposal".to_string());
        state.apply(QuestionFormEvent::Type('Y'));
        assert_eq!(state.editor.as_ref().unwrap().text, "Y");
        assert!(state.drafts[0].is_none());
        state.apply(QuestionFormEvent::NextTab);
        assert!(state.editor.is_none());
        state.apply(QuestionFormEvent::PreviousTab);
        assert_eq!(state.editor.as_ref().unwrap().text, "Y");
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert_eq!(state.drafts[0].as_ref().unwrap().answer, "Y");
        assert_eq!(state.drafts[0].as_ref().unwrap().source, QuestionAnswerSource::Text);
    }

    #[test]
    fn typed_modal_commands_retry_until_explicitly_escaped_as_literal_answers() {
        let mut state = state(1);
        state.apply(QuestionFormEvent::Type(':'));
        state.apply(QuestionFormEvent::ReplaceEditor(":command /status".to_string()));
        assert!(state.apply(QuestionFormEvent::Choose).is_none());
        assert!(state.error.is_some());
        state.apply(QuestionFormEvent::ReplaceEditor("text: :command /status".to_string()));
        assert_eq!(state.apply(QuestionFormEvent::Choose), Some(QuestionFormOutcome::Answered(vec![QuestionAnswer { answer: ":command /status".to_string(), source: QuestionAnswerSource::Text }])));
    }

}
