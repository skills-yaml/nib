//! Presentation-neutral question forms and line input. Answers never authorize tools.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormQuestion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_answer: Option<String>,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionForm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub questions: Vec<FormQuestion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAnswerSource {
    Option,
    Text,
    ApprovedProposal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub answer: String,
    pub source: QuestionAnswerSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestionFormOutcome {
    Answered(Vec<QuestionAnswer>),
    Discussed(String),
    LeftUnanswered,
    Cancelled,
    InputClosed,
    InputUnavailable(String),
}
impl QuestionFormOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Answered(_) | Self::Discussed(_))
    }
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Answered(_) => "answered",
            Self::Discussed(_) => "discussed",
            Self::LeftUnanswered => "left_unanswered",
            Self::Cancelled => "cancelled",
            Self::InputClosed => "input_closed",
            Self::InputUnavailable(_) => "input_unavailable",
        }
    }
}

fn bounded_string(
    value: &Value,
    name: &str,
    limit: usize,
    required: bool,
) -> Result<Option<String>, String> {
    let Some(value) = value.get(name) else {
        return if required {
            Err(format!("ask_question requires {name}"))
        } else {
            Ok(None)
        };
    };
    let text = value
        .as_str()
        .ok_or_else(|| format!("ask_question {name} must be a string"))?;
    if text.len() > limit || (!matches!(name, "header" | "description") && text.trim().is_empty()) {
        return Err(format!(
            "ask_question {name} is empty or exceeds its byte limit"
        ));
    }
    Ok(Some(text.to_string()))
}

fn parse_options(value: &Value) -> Result<Vec<QuestionOption>, String> {
    let Some(value) = value.get("options") else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .filter(|items| items.len() <= 20)
        .ok_or_else(|| "ask_question options must contain at most 20 choices".to_string())?;
    let mut labels = BTreeSet::new();
    array
        .iter()
        .map(|option| {
            let parsed = if let Some(label) = option.as_str() {
                if label.trim().is_empty() || label.len() > 1_000 {
                    return Err(
                        "ask_question string option is empty or exceeds its byte limit".to_string(),
                    );
                }
                QuestionOption {
                    label: label.to_string(),
                    description: None,
                }
            } else {
                let object = option
                    .as_object()
                    .ok_or_else(|| "ask_question option must be a string or object".to_string())?;
                if object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "label" | "description"))
                {
                    return Err("ask_question option contains an unknown field".to_string());
                }
                QuestionOption {
                    label: bounded_string(option, "label", 200, true)?.expect("required label"),
                    description: bounded_string(option, "description", 1_000, false)?,
                }
            };
            if !labels.insert(parsed.label.clone()) {
                return Err("ask_question option labels must be unique".to_string());
            }
            Ok(parsed)
        })
        .collect()
}

fn parse_form_question(value: &Value, title_required: bool) -> Result<FormQuestion, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "ask_question question must be an object".to_string())?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "question" | "title" | "proposed_answer" | "options"
        )
    }) {
        return Err("ask_question question contains an unknown field".to_string());
    }
    Ok(FormQuestion {
        title: bounded_string(value, "title", 40, title_required)?,
        question: bounded_string(value, "question", 20_000, true)?.expect("required question"),
        proposed_answer: bounded_string(value, "proposed_answer", 20_000, false)?,
        options: parse_options(value)?,
    })
}

pub fn parse_question_form(arguments: &Value) -> Result<QuestionForm, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "ask_question arguments must be an object".to_string())?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "question" | "questions" | "header" | "options" | "proposed_answer" | "dependent_paths"
        )
    }) {
        return Err("ask_question arguments contain an unknown field".to_string());
    }
    let questions = match (arguments.get("question"), arguments.get("questions")) {
        (Some(_), Some(_)) | (None, None) => {
            return Err("ask_question requires exactly one of question or questions".to_string())
        }
        (Some(_), None) => vec![FormQuestion {
            title: None,
            question: bounded_string(arguments, "question", 20_000, true)?
                .expect("required question"),
            proposed_answer: bounded_string(arguments, "proposed_answer", 20_000, false)?,
            options: parse_options(arguments)?,
        }],
        (None, Some(value)) => {
            if arguments.get("proposed_answer").is_some() || arguments.get("options").is_some() {
                return Err(
                    "ask_question set options and proposals belong inside each question"
                        .to_string(),
                );
            }
            let items = value
                .as_array()
                .filter(|items| !items.is_empty() && items.len() <= 8)
                .ok_or_else(|| {
                    "ask_question questions must contain one to eight questions".to_string()
                })?;
            items
                .iter()
                .map(|item| parse_form_question(item, items.len() > 1))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    let form = QuestionForm {
        header: bounded_string(arguments, "header", 500, false)?,
        questions,
    };
    validate_display_identity(&form)?;
    Ok(form)
}

fn validate_display_identity(form: &QuestionForm) -> Result<(), String> {
    if form.questions.is_empty()
        || form.questions.len() > 8
        || (form.questions.len() > 1
            && form
                .questions
                .iter()
                .any(|question| question.title.is_none()))
    {
        return Err(
            "ask_question form requires one to eight questions and titles for sets".to_string(),
        );
    }
    if form.header.as_ref().is_some_and(|text| text.len() > 500)
        || form.questions.iter().any(|question| {
            question.question.len() > 20_000
                || question
                    .proposed_answer
                    .as_ref()
                    .is_some_and(|text| text.trim().is_empty() || text.len() > 20_000)
                || question.title.as_ref().is_some_and(|text| text.len() > 40)
                || question.options.len() > 20
                || question.options.iter().any(|option| {
                    option.label.len()
                        > if option.description.is_some() {
                            200
                        } else {
                            1_000
                        }
                        || option
                            .description
                            .as_ref()
                            .is_some_and(|text| text.len() > 1_000)
                })
        })
    {
        return Err("ask_question displayed form exceeds its field limits".to_string());
    }
    let mut titles = BTreeSet::new();
    for question in &form.questions {
        if let Some(title) = &question.title {
            if title.trim().is_empty() || !titles.insert(title) {
                return Err("ask_question displayed titles must be nonempty and unique".to_string());
            }
        }
        let mut labels = BTreeSet::new();
        if question.question.trim().is_empty()
            || question
                .options
                .iter()
                .any(|option| option.label.trim().is_empty() || !labels.insert(&option.label))
        {
            return Err(
                "ask_question displayed question and option labels must be nonempty and unique"
                    .to_string(),
            );
        }
    }
    Ok(())
}

pub fn public_question_form(
    form: &QuestionForm,
    sensitive_values: &[String],
) -> Result<QuestionForm, String> {
    validate_display_identity(form)?;
    let bound =
        |text: &str, bytes| super::bounded_public_text(text, sensitive_values, bytes, false);
    let safe = QuestionForm {
        header: form.header.as_ref().map(|text| bound(text, 500)),
        questions: form
            .questions
            .iter()
            .map(|question| FormQuestion {
                title: question.title.as_ref().map(|text| bound(text, 40)),
                question: bound(&question.question, 20_000),
                proposed_answer: question
                    .proposed_answer
                    .as_ref()
                    .map(|text| bound(text, 20_000)),
                options: question
                    .options
                    .iter()
                    .map(|option| QuestionOption {
                        label: bound(
                            &option.label,
                            if option.label.len() <= 200 {
                                200
                            } else {
                                1_000
                            },
                        ),
                        description: option.description.as_ref().map(|text| bound(text, 1_000)),
                    })
                    .collect(),
            })
            .collect(),
    };
    validate_display_identity(&safe)?;
    Ok(safe)
}

impl QuestionForm {
    pub fn tool_arguments(&self) -> Value {
        let wire_question = |question: &FormQuestion| {
            let mut value =
                json!({"question": question.question, "options": question.wire_options()});
            if let Some(title) = &question.title {
                value["title"] = json!(title);
            }
            if let Some(answer) = &question.proposed_answer {
                value["proposed_answer"] = json!(answer);
            }
            value
        };
        let mut value = if self.questions.len() == 1 && self.questions[0].title.is_none() {
            wire_question(&self.questions[0])
        } else {
            json!({"questions": self.questions.iter().map(wire_question).collect::<Vec<_>>()})
        };
        if let Some(header) = &self.header {
            value["header"] = json!(header);
        }
        value
    }
    pub fn observation(&self, outcome: &QuestionFormOutcome) -> Result<Value, String> {
        validate_form_outcome(self, outcome)?;
        match outcome {
            QuestionFormOutcome::Answered(answers) if self.questions.len() == 1 => {
                let question = &self.questions[0];
                let answer = &answers[0];
                Ok(
                    json!({"status":"answered","question":question.question,"options":question.wire_options(),"answer":answer.answer,"source":answer.source}),
                )
            }
            QuestionFormOutcome::Answered(answers) => Ok(
                json!({"status":"answered","answers": self.questions.iter().zip(answers).map(|(q,a)| json!({"title":q.title,"question":q.question,"answer":a.answer,"source":a.source})).collect::<Vec<_>>()}),
            ),
            QuestionFormOutcome::Discussed(message) => Ok(
                json!({"status":"discussed","message":message,"questions":self.questions.iter().map(|q| &q.question).collect::<Vec<_>>()}),
            ),
            other => Err(match other {
                QuestionFormOutcome::InputUnavailable(error) => error.clone(),
                QuestionFormOutcome::InputClosed => {
                    "question input closed before a response was received".to_string()
                }
                _ => other.reason().to_string(),
            }),
        }
    }
}
impl FormQuestion {
    pub fn wire_options(&self) -> Value {
        Value::Array(
            self.options
                .iter()
                .map(|option| match &option.description {
                    None => json!(option.label),
                    Some(description) => json!({"label":option.label,"description":description}),
                })
                .collect(),
        )
    }
}

pub fn validate_question_answer(
    question: &FormQuestion,
    answer: &QuestionAnswer,
) -> Result<(), String> {
    if answer.answer.trim().is_empty() || answer.answer.len() > 20_000 {
        return Err("question answer is empty or exceeds its byte limit".to_string());
    }
    match answer.source {
        QuestionAnswerSource::ApprovedProposal
            if question.proposed_answer.as_deref() != Some(answer.answer.as_str()) =>
        {
            Err("approved proposal did not match the displayed answer".to_string())
        }
        QuestionAnswerSource::Option
            if question.proposed_answer.is_some()
                || !question
                    .options
                    .iter()
                    .any(|option| option.label == answer.answer) =>
        {
            Err("question answer did not match a displayed option".to_string())
        }
        _ => Ok(()),
    }
}
pub fn validate_form_outcome(
    form: &QuestionForm,
    outcome: &QuestionFormOutcome,
) -> Result<(), String> {
    match outcome {
        QuestionFormOutcome::Answered(answers) if answers.len() != form.questions.len() => {
            Err("question form requires every answer before Submit".to_string())
        }
        QuestionFormOutcome::Answered(answers) => form
            .questions
            .iter()
            .zip(answers)
            .try_for_each(|(q, a)| validate_question_answer(q, a)),
        QuestionFormOutcome::Discussed(message)
            if message.trim().is_empty() || message.len() > 20_000 =>
        {
            Err("discussion message is empty or exceeds its byte limit".to_string())
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionLineInput {
    Answer(QuestionAnswer),
    EnterText,
    EnterDiscussion,
    Reject,
    Interrupt,
    Retry(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionEditorInput {
    Text(String),
    Interrupt,
    Retry(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionSubmitInput {
    Submit,
    Reopen(usize),
    EnterDiscussion,
    Interrupt,
    Retry(String),
}

pub fn parse_question_editor_input(input: &str) -> QuestionEditorInput {
    let input = input.trim();
    if let Some(text) = input.strip_prefix("text:") {
        return match super::parse_prefixed_payload("text:", text) {
            Ok(text) => QuestionEditorInput::Text(text),
            Err(error) => QuestionEditorInput::Retry(error),
        };
    }
    if input == "esc" {
        QuestionEditorInput::Interrupt
    } else if input.is_empty() {
        QuestionEditorInput::Retry("text cannot be empty".to_string())
    } else {
        QuestionEditorInput::Text(input.to_string())
    }
}

pub fn parse_question_line(question: &FormQuestion, input: &str) -> QuestionLineInput {
    let input = input.trim();
    if input.starts_with("text:") {
        return match parse_question_editor_input(input) {
            QuestionEditorInput::Text(answer) => QuestionLineInput::Answer(QuestionAnswer {
                answer,
                source: QuestionAnswerSource::Text,
            }),
            QuestionEditorInput::Retry(error) => QuestionLineInput::Retry(error),
            QuestionEditorInput::Interrupt => unreachable!(),
        };
    }
    if input == "esc" {
        return QuestionLineInput::Interrupt;
    }
    if input == "chat" {
        return QuestionLineInput::EnterDiscussion;
    }
    if input.is_empty() {
        return QuestionLineInput::Retry("question response cannot be empty".to_string());
    }
    if let Some(proposal) = &question.proposed_answer {
        return match input {
            "1" | "approve" | "yes" => QuestionLineInput::Answer(QuestionAnswer {
                answer: proposal.clone(),
                source: QuestionAnswerSource::ApprovedProposal,
            }),
            "2" | "reject" | "no" => QuestionLineInput::Reject,
            "3" | "otherwise" | "instruct otherwise" => QuestionLineInput::EnterText,
            "4" => QuestionLineInput::EnterDiscussion,
            _ if input.chars().all(|c| c.is_ascii_digit()) => {
                QuestionLineInput::Retry("question row is out of range".to_string())
            }
            _ => QuestionLineInput::Answer(QuestionAnswer {
                answer: input.to_string(),
                source: QuestionAnswerSource::Text,
            }),
        };
    }
    if input.chars().all(|c| c.is_ascii_digit()) {
        let row = input.parse::<usize>().unwrap_or(usize::MAX);
        if row == question.options.len() + 1 {
            return QuestionLineInput::EnterText;
        }
        if row == question.options.len() + 2 {
            return QuestionLineInput::EnterDiscussion;
        }
        return row
            .checked_sub(1)
            .and_then(|index| question.options.get(index))
            .map(|option| {
                QuestionLineInput::Answer(QuestionAnswer {
                    answer: option.label.clone(),
                    source: QuestionAnswerSource::Option,
                })
            })
            .unwrap_or_else(|| {
                QuestionLineInput::Retry("question row is out of range".to_string())
            });
    }
    QuestionLineInput::Answer(QuestionAnswer {
        answer: input.to_string(),
        source: QuestionAnswerSource::Text,
    })
}

pub fn parse_question_submit_line(input: &str, question_count: usize) -> QuestionSubmitInput {
    match input.trim() {
        "" => QuestionSubmitInput::Submit,
        "chat" => QuestionSubmitInput::EnterDiscussion,
        "esc" => QuestionSubmitInput::Interrupt,
        text => text
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0 && *n <= question_count)
            .map(|n| QuestionSubmitInput::Reopen(n - 1))
            .unwrap_or_else(|| {
                QuestionSubmitInput::Retry(
                    "Enter submits; a question number reopens that question".to_string(),
                )
            }),
    }
}
